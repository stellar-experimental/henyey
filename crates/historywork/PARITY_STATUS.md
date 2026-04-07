# stellar-core Parity Status

**Crate**: `henyey-historywork`
**Upstream**: `stellar-core/src/historywork/`
**Overall Parity**: 38%
**Last Updated**: 2026-04-07

## Summary

| Area | Status | Notes |
|------|--------|-------|
| History archive state fetch | Full | Native async HAS download with shared-state storage |
| Bucket download and verification | Full | Parallel download with SHA-256 hash verification |
| Batch checkpoint file downloads | Full | Headers, txs, results, SCP downloaded in parallel |
| Ledger header chain verification | Full | Hash-chain integrity verified on download |
| Transaction set verification | Full | Hash verified against ledger header |
| Transaction result verification | Full | Hash verified against ledger header |
| Progress reporting | Full | Stage enums and status messages exposed |
| Publish pipeline | None | Snapshot write/resolve/upload orchestration missing |
| Verified checkpoint hash export | None | Offline verified hash chain writer missing |
| Recent quorum-set fetch | None | Bootstrap SCP qset fetcher missing |
| Checkpoint-range tx result verification | None | No batch download+verify for ranges |
| Single ledger header check | None | Offline self-check work missing |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `GetHistoryArchiveStateWork.h/.cpp` | `download.rs` (`GetHistoryArchiveStateWork`) | Full — fetches and stores HAS in shared state |
| `DownloadBucketsWork.h/.cpp` | `download.rs` (`DownloadBucketsWork`) | Full — parallel download with hash verification |
| `VerifyBucketWork.h/.cpp` | `download.rs` (`download_and_save_bucket`) | Full — hash verification inlined into download |
| `BatchDownloadWork.h/.cpp` | `download.rs` (individual work items) | Full — concept distributed across download work items |
| `VerifyTxResultsWork.h/.cpp` | `download.rs` (`DownloadTxResultsWork`) | Full — verification folded into result download |
| `Progress.h/.cpp` | `lib.rs` (`HistoryWorkProgress`, `set_progress`, `get_progress`) | Full — progress tracking via shared state |
| `CheckSingleLedgerHeaderWork.h/.cpp` | — | Not implemented |
| `DownloadVerifyTxResultsWork.h/.cpp` | — | Not implemented |
| `FetchRecentQsetsWork.h/.cpp` | — | Not implemented |
| `PublishWork.h/.cpp` | — | Not implemented |
| `PutFilesWork.h/.cpp` | — | Not implemented |
| `PutHistoryArchiveStateWork.h/.cpp` | — | Not implemented |
| `PutSnapshotFilesWork.h/.cpp` | — | Not implemented |
| `ResolveSnapshotWork.h/.cpp` | — | Not implemented |
| `WriteSnapshotWork.h/.cpp` | — | Not implemented |
| `WriteVerifiedCheckpointHashesWork.h/.cpp` | — | Not implemented |

## Component Mapping

### GetHistoryArchiveStateWork (`download.rs`)

Corresponds to: `GetHistoryArchiveStateWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `GetHistoryArchiveStateWork(app, seq, archive, report, maxRetries)` | `GetHistoryArchiveStateWork { archive, checkpoint, state }` | Full |
| `getHistoryArchiveState()` | `SharedHistoryState.has` | Full |
| `getArchive()` | `GetHistoryArchiveStateWork.archive` | Full |
| `getStatus()` | `get_progress()` / `HistoryWorkProgress` | Full |
| `doWork()` | `Work::run()` | Full |
| `doReset()` | Not needed — work items are stateless between retries | Full |
| `onSuccess()` (metric reporting) | Not implemented — no medida metrics | None |

### DownloadBucketsWork (`download.rs`)

Corresponds to: `DownloadBucketsWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `DownloadBucketsWork(app, liveBuckets, hotBuckets, liveHashes, hotHashes, downloadDir, archive)` | `DownloadBucketsWork { archive, state, bucket_dir }` | Full |
| `hasNext()` / `yieldMoreWork()` / `resetIter()` | `Work::run()` with `stream::buffer_unordered` | Full |
| `getStatus()` | `get_progress()` | Full |
| `onSuccessCb()` (bucket adoption into BucketManager) | Not implemented — bucket files saved to disk only | Partial |

### VerifyBucketWork (`download.rs`)

Corresponds to: `VerifyBucketWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `VerifyBucketWork(app, bucketFile, hash, index, failureCb)` | `download_and_save_bucket(archive, hash, path)` | Full |
| `onRun()` / `spawnVerifier()` (hash verification) | `verify::verify_bucket_hash()` | Full |
| `onFailureRaise()` (failure callback) | Error propagated via `Result` | Full |
| Bucket index creation | Not implemented | None |

### BatchDownloadWork (`download.rs`)

Corresponds to: `BatchDownloadWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BatchDownloadWork(app, range, type, downloadDir, archive)` | Individual download work items per file type | Full |
| `hasNext()` / `yieldMoreWork()` / `resetIter()` | `Work::run()` with direct archive API calls | Full |
| `getStatus()` | `get_progress()` | Full |

### DownloadLedgerHeadersWork (`download.rs`)

Corresponds to: `BatchDownloadWork.h` (for `HISTORY_FILE_TYPE_LEDGER`)

| stellar-core | Rust | Status |
|--------------|------|--------|
| Batch download + gunzip for ledger headers | `DownloadLedgerHeadersWork::run()` | Full |
| Header chain verification | `verify::verify_header_chain_from_entries()` | Full |

### DownloadTransactionsWork (`download.rs`)

Corresponds to: `BatchDownloadWork.h` (for `HISTORY_FILE_TYPE_TRANSACTIONS`)

| stellar-core | Rust | Status |
|--------------|------|--------|
| Batch download + gunzip for transactions | `DownloadTransactionsWork::run()` | Full |
| Transaction set hash verification | `verify::verify_tx_set()` | Full |

### DownloadTxResultsWork (`download.rs`)

Corresponds to: `BatchDownloadWork.h` (for `HISTORY_FILE_TYPE_RESULTS`) + `VerifyTxResultsWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| Batch download + gunzip for results | `DownloadTxResultsWork::run()` | Full |
| `verifyTxResultsOfCheckpoint()` | `verify::verify_tx_result_set()` | Full |
| `getCurrentTxResultSet()` | Inline in `run()` | Full |

### DownloadScpHistoryWork (`download.rs`)

Corresponds to: `BatchDownloadWork.h` (for `HISTORY_FILE_TYPE_SCP`)

| stellar-core | Rust | Status |
|--------------|------|--------|
| Batch download + gunzip for SCP messages | `DownloadScpHistoryWork::run()` | Full |

### Progress (`lib.rs`)

Corresponds to: `Progress.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `fmtProgress(app, task, range, curr)` | `HistoryWorkProgress` / `set_progress()` / `get_progress()` | Full |

### HistoryWorkBuilder (`builder.rs`)

No direct upstream equivalent — Rust-specific DAG registration.

| Rust | Notes |
|------|-------|
| `HistoryWorkBuilder::new()` | Configures archive, checkpoint, state, bucket dir |
| `HistoryWorkBuilder::register()` | Registers work DAG with `WorkScheduler` |
| `HistoryWorkIds` | Returns work IDs for downstream queries |

### CheckSingleLedgerHeaderWork

Corresponds to: `CheckSingleLedgerHeaderWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `CheckSingleLedgerHeaderWork(app, archive, entry)` | — | None |
| `doWork()` / `doReset()` / `onFailureRaise()` / `onSuccess()` | — | None |

### DownloadVerifyTxResultsWork

Corresponds to: `DownloadVerifyTxResultsWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `DownloadVerifyTxResultsWork(app, range, downloadDir, archive)` | — | None |
| `hasNext()` / `yieldMoreWork()` / `resetIter()` / `getStatus()` | — | None |

### FetchRecentQsetsWork

Corresponds to: `FetchRecentQsetsWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `FetchRecentQsetsWork(app, ledgerNum)` | — | None |
| `doWork()` / `doReset()` | — | None |

### PublishWork

Corresponds to: `PublishWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PublishWork(app, snapshot, seq, bucketHashes)` | — | None |
| `onFailureRaise()` / `onSuccess()` | — | None |

### PutFilesWork

Corresponds to: `PutFilesWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PutFilesWork(app, archive, snapshot, remoteState)` | — | None |
| `doWork()` / `doReset()` | — | None |

### PutHistoryArchiveStateWork

Corresponds to: `PutHistoryArchiveStateWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PutHistoryArchiveStateWork(app, state, archive)` | — | None |
| `doWork()` / `doReset()` | — | None |

### PutSnapshotFilesWork

Corresponds to: `PutSnapshotFilesWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PutSnapshotFilesWork(app, snapshot)` | — | None |
| `doWork()` / `doReset()` / `onSuccess()` / `getStatus()` | — | None |

### ResolveSnapshotWork

Corresponds to: `ResolveSnapshotWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `ResolveSnapshotWork(app, snapshot)` | — | None |
| `onRun()` / `onAbort()` | — | None |

### WriteSnapshotWork

Corresponds to: `WriteSnapshotWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `WriteSnapshotWork(app, snapshot)` | — | None |
| `onRun()` / `onAbort()` | — | None |

### WriteVerifiedCheckpointHashesWork

Corresponds to: `WriteVerifiedCheckpointHashesWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `WriteVerifiedCheckpointHashesWork(app, rangeEnd, outputFile, ...)` | — | None |
| `loadHashFromJsonOutput()` | — | None |
| `loadLatestHashPairFromJsonOutput()` | — | None |
| `hasNext()` / `yieldMoreWork()` / `resetIter()` / `onSuccess()` | — | None |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `RunCommandWork` | Rust uses native async I/O instead of subprocess-driven shell commands |
| `GetRemoteFileWork` | HTTP download handled by `reqwest` in `henyey-history` |
| `GetAndUnzipRemoteFileWork` | Download + decompress performed natively with `flate2`, not chained shell work |
| `GunzipFileWork` | Decompression uses `flate2` crate instead of `gunzip` subprocesses |
| `GzipFileWork` | Compression uses `flate2` crate instead of `gzip` subprocesses |
| `PutRemoteFileWork` | Upload abstracted behind the `ArchiveWriter` trait in `henyey-history` |
| `MakeRemoteDirWork` | Archive writer implementations create directories implicitly |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `CheckSingleLedgerHeaderWork` | Low | Offline self-check command; not needed for catchup or watcher |
| `DownloadVerifyTxResultsWork` | Medium | Checkpoint-range tx result download+verify orchestration |
| `FetchRecentQsetsWork` | Low | SCP quorum-set bootstrap fetcher; not needed for current workflows |
| `PublishWork` | Medium | Top-level publish orchestration and callbacks |
| `PutFilesWork` | Medium | Differential upload against remote HAS |
| `PutHistoryArchiveStateWork` | Medium | Publishing HAS JSON to archive |
| `PutSnapshotFilesWork` | Medium | Snapshot gzip/upload pipeline |
| `ResolveSnapshotWork` | Medium | Snapshot bucket-reference resolution |
| `WriteSnapshotWork` | Medium | Live-state snapshot writer for publish workflow |
| `WriteVerifiedCheckpointHashesWork` | Low | Offline verified hash-chain export tool |

## Architectural Differences

1. **Transport and compression**
   - **stellar-core**: Builds history tasks from shell-command work items (`curl`, `gzip`, `gunzip`) via `RunCommandWork`.
   - **Rust**: Uses native HTTP (`reqwest`) and compression (`flate2`) libraries directly inside async work items.
   - **Rationale**: Avoids subprocess overhead and integrates naturally with the tokio-based scheduler.

2. **Work scheduling model**
   - **stellar-core**: Uses `Work`/`BatchWork`/`WorkSequence` state machines with child-work spawning and cranking.
   - **Rust**: Registers work items as an explicit DAG via `HistoryWorkBuilder` + `WorkScheduler`.
   - **Rationale**: Makes dependency ordering explicit and composable without nested state machines.

3. **Shared state vs. return values**
   - **stellar-core**: Each work item stores results internally; callers access them via getters after completion.
   - **Rust**: All work items write to a shared `HistoryWorkState` behind an `Arc<Mutex<...>>`.
   - **Rationale**: Simplifies data flow in a DAG scheduler where work items don't have parent-child relationships.

4. **Bucket handling**
   - **stellar-core**: `VerifyBucketWork` verifies hashes and builds bucket indexes, then `DownloadBucketsWork::onSuccessCb` adopts buckets into `BucketManager`.
   - **Rust**: `download_and_save_bucket` verifies hashes and saves files to disk; bucket-manager adoption is handled separately during catchup.
   - **Rationale**: Bucket index building is a BucketManager concern; the download work only needs verified files on disk.

5. **Publish vs. download focus**
   - **stellar-core**: The `historywork` directory covers both download (catchup) and publish (archiver) workflows.
   - **Rust**: `henyey-historywork` currently implements only the download/verification side.
   - **Rationale**: Current use cases (watcher, catchup, offline verification) require only downloads. Publish pipeline will be added when archiver support is needed.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Verified checkpoint hashes | 1 TEST_CASE / 2 SECTION | 0 `#[test]` | Feature not implemented in Rust |
| Single ledger header check | 1 TEST_CASE / 0 SECTION | 0 `#[test]` | Feature not implemented in Rust |
| Download work chain | Indirect via CatchupSimulation | 1 `#[tokio::test]` | `history_work.rs` exercises full download DAG |
| Checkpoint data assembly | — | 2 `#[tokio::test]` | `checkpoint_data.rs` covers success and failure paths |
| Retry constants | — | 2 `#[test]` | Verifies RETRY_A_FEW=5 and RETRY_A_LOT=32 match upstream |

### Test Gaps

- `WriteVerifiedCheckpointHashesWork` has upstream acceptance-level tests (1 TEST_CASE / 2 SECTION) with no Rust equivalent.
- `CheckSingleLedgerHeaderWork` has an upstream test (1 TEST_CASE) with no Rust equivalent.
- The full publish pipeline (snapshot write, resolve, upload) has no Rust implementation or tests.
- No dedicated error-path or retry-behavior tests for individual download work items.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 6 |
| Gaps (None + Partial) | 10 |
| Intentional Omissions | 7 |
| **Parity** | **6 / (6 + 10) = 38%** |

Full components: GetHistoryArchiveStateWork, DownloadBucketsWork, VerifyBucketWork, BatchDownloadWork, VerifyTxResultsWork, Progress.

Gap components: CheckSingleLedgerHeaderWork, DownloadVerifyTxResultsWork, FetchRecentQsetsWork, PublishWork, PutFilesWork, PutHistoryArchiveStateWork, PutSnapshotFilesWork, ResolveSnapshotWork, WriteSnapshotWork, WriteVerifiedCheckpointHashesWork.
