# stellar-core Parity Status

**Crate**: `henyey`
**Upstream**: `stellar-core/src/main/` (CLI subset only)
**Overall Parity**: 51%
**Last Updated**: 2026-03-06

## Summary

| Area | Status | Notes |
|------|--------|-------|
| CLI Entrypoint / Dispatch | Full | `main()` + `handleCommandLine()` via clap |
| Node Operation (`run`) | Full | Watcher/validator/full modes |
| Catchup (`catchup`) | Full | Minimal/complete/recent modes, parallelism |
| Database Commands | Full | `new-db`, `upgrade-db` |
| History Publish (`publish`) | Full | Local and remote (put command) archives |
| History Verification | Full | `verify-checkpoints`, `verify-history` |
| Key Generation | Full | `new-keypair` (genSeed equivalent) |
| HTTP Command | Full | `http-command` to local node |
| Self-Check | Full | Header chain, bucket hash, crypto benchmark |
| Dump Ledger | Full | `dump-ledger` with type/modified filters |
| Quorum Intersection | Partial | V1 brute-force only; V2 SAT-solver not implemented |
| Key/Crypto Utilities | Partial | `convert-id` implemented; `sec-to-pub`, `sign-transaction` not yet |
| XDR Tools | None | `decode-xdr`/`encode-xdr` removed; `dump-xdr` not implemented |
| Diagnostic CLI Commands | None | `diag-bucket-stats`, `merge-bucketlist`, `dump-archival-stats` etc. |
| Settings Upgrade Transactions | None | `get-settings-upgrade-txs` not implemented |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `CommandLine.h` / `CommandLine.cpp` | `main.rs` | CLI commands and argument parsing |
| `main.cpp` | `main.rs` | Process entrypoint, initialization |
| `ApplicationUtils.h` / `ApplicationUtils.cpp` | `main.rs` | run, catchup, publish, selfCheck, dumpLedger |
| `StellarCoreVersion.h` | `main.rs` (via `clap` version) | Version string |
| `dumpxdr.h` / `dumpxdr.cpp` | -- | Removed from Rust; was partial |

## Component Mapping

### CLI Entrypoint (`main.rs`)

Corresponds to: `CommandLine.h`, `main.cpp`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `main()` | `main()` | Full |
| `handleCommandLine()` | `main()` + clap dispatch | Full |
| `runVersion()` | `cmd_version()` | Full |
| `writeWithTextFlow()` | clap help formatting | Full |
| `checkXDRFileIdentity()` | -- | Omitted |
| `checkStellarCoreMajorVersionProtocolIdentity()` | -- | Omitted |

### CLI Commands (`main.rs`)

Corresponds to: `CommandLine.cpp`

#### Node Operation

| stellar-core | Rust | Status |
|--------------|------|--------|
| `run()` | `cmd_run()` | Full |
| `runCatchup()` | `cmd_catchup()` | Full |
| `runNewDB()` | `cmd_new_db()` | Full |
| `runUpgradeDB()` | `cmd_upgrade_db()` | Full |
| `runPublish()` | `cmd_publish_history()` | Full |
| `runOfflineInfo()` | `cmd_offline_info()` | Full |
| `runSelfCheck()` | `cmd_self_check()` | Full |
| `runHttpCommand()` | `cmd_http_command()` | Full |

#### History and Verification

| stellar-core | Rust | Status |
|--------------|------|--------|
| `runWriteVerifiedCheckpointHashes()` | `cmd_verify_checkpoints()` | Full |
| `runCheckQuorumIntersection()` | `cmd_check_quorum_intersection()` | Partial |
| `runNewHist()` | `cmd_new_hist()` | Full |
| `runReportLastHistoryCheckpoint()` | -- | None |
| `runPrintPublishQueue()` | -- | None |

#### Key and Crypto Utilities

| stellar-core | Rust | Status |
|--------------|------|--------|
| `runGenSeed()` | `cmd_new_keypair()` | Full |
| `runConvertId()` | `cmd_convert_id()` | Full |
| `runSecToPub()` | -- | None |
| `runSignTransaction()` | -- | None |

#### XDR and Diagnostic Tools

| stellar-core | Rust | Status |
|--------------|------|--------|
| `runDumpLedger()` | `cmd_dump_ledger()` | Full |
| `runPrintXdr()` | -- | None |
| `runEncodeAsset()` | -- | None |
| `runDumpXDR()` | -- | None |
| `runDumpWasm()` | -- | None |
| `diagBucketStats()` | -- | None |
| `runMergeBucketList()` | -- | None |
| `runDumpStateArchivalStatistics()` | -- | None |

#### Other Commands

| stellar-core | Rust | Status |
|--------------|------|--------|
| `runForceSCP()` | `cmd_force_scp()` | Full |
| `runReplayDebugMeta()` | -- | None |
| `getSettingsUpgradeTransactions()` | -- | None |

### Quorum Intersection (`quorum_intersection.rs`)

Corresponds to: `CommandLine.cpp` (`runCheckQuorumIntersection`)

| stellar-core | Rust | Status |
|--------------|------|--------|
| V1 brute-force intersection check | `check_quorum_intersection_from_json()` | Full |
| V2 SAT-based intersection check | -- | None |
| `--analyze-critical-groups` | -- | None |
| `--time-limit-ms` / `--memory-limit-bytes` | -- | None |
| `--result-json` output | -- | None |

### XDR Utilities

Corresponds to: `dumpxdr.h` / `dumpxdr.cpp`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `dumpXdrStream()` | -- | None |
| `printXdr()` | -- | None |
| `signtxns()` | -- | None |
| `signtxn()` | -- | None |
| `priv2pub()` | -- | None |
| `readFile()` | -- | None |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `checkXDRFileIdentity()` | Pure Rust implementation has no C++/Rust XDR bridge to validate |
| `checkStellarCoreMajorVersionProtocolIdentity()` | Different versioning scheme; Rust uses Cargo.toml version |
| `runLoadXDR()` (BUILD_TESTS) | Test-only; Rust test framework used instead |
| `runRebuildLedgerFromBuckets()` (BUILD_TESTS) | Test-only infrastructure |
| `runFuzz()` / `runGenFuzz()` (BUILD_TESTS) | Would use `cargo-fuzz` if needed |
| `runTest()` (BUILD_TESTS) | Uses `cargo test` instead |
| `runApplyLoad()` (BUILD_TESTS) | Benchmarking infrastructure; out of scope |
| `runGenerateSyntheticLoad()` (BUILD_TESTS) | Test load generation; out of scope |
| Tracy memory tracking | C++-specific profiling; Rust uses different tooling |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `runSecToPub()` / `priv2pub()` | Medium | Derive public key from secret (removed from Rust, should re-add) |
| `runSignTransaction()` / `signtxn()` | Medium | Sign a transaction envelope (removed from Rust, should re-add) |
| `runPrintXdr()` / `printXdr()` | Low | Decode and pretty-print XDR (removed from Rust) |
| `runEncodeAsset()` | Low | Encode asset to XDR (removed from Rust) |
| `runReplayDebugMeta()` | Low | Apply ledgers from local debug metadata files |
| `runDumpXDR()` / `dumpXdrStream()` | Low | Stream and dump XDR files |
| `runDumpWasm()` | Low | Dump WASM blobs from ledger state |
| `diagBucketStats()` | Low | Per-account bucket statistics |
| `runMergeBucketList()` | Low | Write diagnostic merged bucket list |
| `runDumpStateArchivalStatistics()` | Low | Print state archival statistics |
| `runReportLastHistoryCheckpoint()` | Low | Report info about last archive checkpoint |
| `getSettingsUpgradeTransactions()` | Low | Generate settings upgrade transaction set |
| `runPrintPublishQueue()` | Low | Print scheduled publish checkpoints |
| V2 SAT-based quorum intersection | Medium | `check-quorum-intersection --v2` with SAT solver |
| `signtxns()` (batch sign) | Low | Batch transaction signing |
| `readFile()` | Low | Generic XDR file reader |

## Architectural Differences

1. **CLI Framework**
   - **stellar-core**: Uses `clara` (lightweight C++ argument parser) with manual command registration and help formatting.
   - **Rust**: Uses `clap` with derive macros for declarative command definitions and automatic help generation.
   - **Rationale**: `clap` is the standard Rust CLI framework and provides better ergonomics, auto-completion, and type safety.

2. **Command Organization**
   - **stellar-core**: All commands are flat at the top level (`stellar-core <command>`).
   - **Rust**: All commands are also flat at the top level. Previously had an `offline` subcommand grouping but this was flattened to match stellar-core's approach.
   - **Rationale**: Flat commands are simpler and match the upstream CLI experience.

3. **Configuration Loading**
   - **stellar-core**: Uses `ConfigOption` struct with per-command config parsing; config file defaults to `stellar-core.cfg`.
   - **Rust**: Global `--config`, `--testnet`, `--mainnet` flags parsed before command dispatch; supports TOML with built-in network defaults.
   - **Rationale**: Global config flags are simpler and avoid repetition across commands.

4. **Process Initialization**
   - **stellar-core**: `main.cpp` performs XDR identity checks, sodium init, backtrace setup, and version/protocol identity verification before dispatching commands.
   - **Rust**: `main()` initializes logging and loads config; no XDR identity checks needed (pure Rust, single XDR crate). Signal handlers and panic hooks are set up by the Rust runtime.
   - **Rationale**: Pure Rust eliminates the need for C++/Rust XDR bridge validation. Rust's panic infrastructure replaces explicit backtrace management.

5. **Async Runtime**
   - **stellar-core**: Synchronous command execution (blocking I/O via `asio`/virtual clock).
   - **Rust**: `#[tokio::main]` with async command handlers for network operations.
   - **Rationale**: Rust's async ecosystem is idiomatic and provides native non-blocking I/O.

6. **CDP Integration (Rust-only)**
   - **stellar-core**: No equivalent offline verification tools.
   - **Rust**: `verify-execution` and `debug-bucket-entry` commands compare transaction execution against CDP (Crypto Data Platform) metadata from stellar-core production data.
   - **Rationale**: These tools provide parity testing against production stellar-core output without requiring a live network connection.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| CLI Parsing | 0 TEST_CASE | 5 #[test] | Rust has explicit CLI parsing tests |
| Config | 8 TEST_CASE / 18 SECTION | 0 #[test] | Config tests are in `henyey-app` crate |
| Command Handler | 3 TEST_CASE / 25 SECTION | 0 #[test] | HTTP handler tests in `henyey-app` |
| Application Utils | 4 TEST_CASE / 5 SECTION | 0 #[test] | Catchup/run logic tested via integration |
| Self-Check | 1 TEST_CASE | 0 #[test] | Self-check tested manually |
| Query Server | 1 TEST_CASE / 9 SECTION | 0 #[test] | No query server in Rust |
| Quorum Intersection | 0 TEST_CASE | 5 #[test] | Rust has dedicated quorum intersection tests |

### Test Gaps

- **Config tests**: stellar-core has 8 TEST_CASE with 18 SECTIONs for configuration parsing and validation. Config testing in Rust is handled in the `henyey-app` crate, not in this CLI crate.
- **Command handler tests**: stellar-core has 3 TEST_CASE with 25 SECTIONs for HTTP command handler behavior. These tests exist in `henyey-app` for the Rust implementation.
- **Application utils tests**: stellar-core tests catchup configuration parsing, version extraction, and related utilities. Not covered in the CLI crate.

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
| Implemented (Full) | 18 |
| Gaps (None + Partial) | 17 |
| Intentional Omissions | 9 |
| **Parity** | **18 / (18 + 17) = 51%** |

Note: Parity increased from 43% to 51% because `convert-id`, `force-scp`, and `new-hist` were
implemented (previously listed as gaps or omissions). Two medium-priority gaps remain:
`sec-to-pub` and `sign-transaction`.
