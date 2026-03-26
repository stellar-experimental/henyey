# stellar-core Parity Status

**Crate**: `henyey-historywork`
**Upstream**: `stellar-core/src/historywork/`
**Overall Parity**: 38%
**Last Updated**: 2026-03-26

## Summary

| Area | Status | Notes |
|------|--------|-------|
| History archive state fetch | Full | Native HAS download and shared-state storage |
| Bucket download and verification | Partial | Hash verification exists; no bucket-manager adoption/indexes |
| Batch checkpoint downloads | Full | Parallel range downloads for history files |
| Single ledger header verification | Full | Archive header comparison work item exists |
| Transaction result verification | Full | Single-checkpoint verification is implemented inline |
| History archive state publish | Full | Checkpoint JSON plus well-known path |
| Progress reporting | Full | Stage enums and status messages are exposed |
| Batch tx-result verification | None | No range-oriented download+verify work item |
| Snapshot publish pipeline | None | Snapshot write/resolve/upload orchestration missing |
| Verified checkpoint hash export | None | Offline verified hash chain writer missing |
| Recent quorum-set fetch | None | Bootstrap SCP qset fetcher missing |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `GetHistoryArchiveStateWork.h` / `GetHistoryArchiveStateWork.cpp` | `lib.rs` (`GetHistoryArchiveStateWork`) | Full parity for HAS fetch workflow |
| `DownloadBucketsWork.h` / `DownloadBucketsWork.cpp` | `lib.rs` (`DownloadBucketsWork`) | Downloads and hashes buckets, but does not adopt into a bucket manager |
| `VerifyBucketWork.h` / `VerifyBucketWork.cpp` | `lib.rs` (`download_and_save_bucket`) | Inline hash verification only; no bucket index creation |
| `BatchDownloadWork.h` / `BatchDownloadWork.cpp` | `lib.rs` (`BatchDownloadWork`) | Generic checkpoint-range downloader |
| `CheckSingleLedgerHeaderWork.h` / `CheckSingleLedgerHeaderWork.cpp` | `lib.rs` (`CheckSingleLedgerHeaderWork`) | Full parity for single-ledger archive checks |
| `VerifyTxResultsWork.h` / `VerifyTxResultsWork.cpp` | `lib.rs` (`DownloadTxResultsWork`) | Verification folded into single-checkpoint result download |
| `DownloadVerifyTxResultsWork.h` / `DownloadVerifyTxResultsWork.cpp` | `--` | No equivalent range verifier/orchestrator |
| `PutHistoryArchiveStateWork.h` / `PutHistoryArchiveStateWork.cpp` | `lib.rs` (`PublishHistoryArchiveStateWork`) | Publishes checkpoint HAS and `.well-known/stellar-history.json` |
| `Progress.h` / `Progress.cpp` | `lib.rs` (`HistoryWorkProgress`, `BatchDownloadProgress`) | Equivalent progress strings via shared state |
| `WriteSnapshotWork.h` / `WriteSnapshotWork.cpp` | `--` | Not implemented |
| `ResolveSnapshotWork.h` / `ResolveSnapshotWork.cpp` | `--` | Not implemented |
| `PutFilesWork.h` / `PutFilesWork.cpp` | `--` | Not implemented |
| `PutSnapshotFilesWork.h` / `PutSnapshotFilesWork.cpp` | `--` | Not implemented |
| `PublishWork.h` / `PublishWork.cpp` | `--` | Not implemented |
| `WriteVerifiedCheckpointHashesWork.h` / `WriteVerifiedCheckpointHashesWork.cpp` | `--` | Not implemented |
| `FetchRecentQsetsWork.h` / `FetchRecentQsetsWork.cpp` | `--` | Not implemented |

## Component Mapping

### GetHistoryArchiveStateWork (`lib.rs`)

Corresponds to: `GetHistoryArchiveStateWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `GetHistoryArchiveStateWork(...)` | `GetHistoryArchiveStateWork::new(...)` | Full |
| `getHistoryArchiveState()` | `SharedHistoryState.has` | Full |
| `getArchive()` | `GetHistoryArchiveStateWork.archive` | Full |
| `getStatus()` | `get_progress()` / `HistoryWorkProgress` | Full |

### DownloadBucketsWork (`lib.rs`)

Corresponds to: `DownloadBucketsWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `DownloadBucketsWork(...)` | `DownloadBucketsWork::new(...)` | Full |
| `getStatus()` | `get_progress()` / `HistoryWorkProgress` | Full |

Component status: Partial - Rust downloads and hashes all referenced buckets, but it does not hand verified buckets to a bucket manager or retain stellar-core style bucket indexes.

### VerifyBucketWork (`lib.rs`)

Corresponds to: `VerifyBucketWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `VerifyBucketWork(...)` | `download_and_save_bucket(...)` | Partial |

Component status: Partial - hash verification is present, but the standalone verifier/index-builder workflow is collapsed into a simple inline helper.

### BatchDownloadWork (`lib.rs`)

Corresponds to: `BatchDownloadWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BatchDownloadWork(...)` | `BatchDownloadWork::new(...)` | Full |
| `getStatus()` | `BatchDownloadProgress::message()` | Full |

### CheckSingleLedgerHeaderWork (`lib.rs`)

Corresponds to: `CheckSingleLedgerHeaderWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `CheckSingleLedgerHeaderWork(...)` | `CheckSingleLedgerHeaderWork::new(...)` | Full |

### VerifyTxResultsWork (`lib.rs`)

Corresponds to: `VerifyTxResultsWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `VerifyTxResultsWork(...)` | `DownloadTxResultsWork::new(...)` | Full |

Component status: Full - Rust performs the same per-checkpoint tx-result hash validation, but folds it into the result-download work item instead of exposing a separate verifier type.

### DownloadVerifyTxResultsWork (`lib.rs`)

Corresponds to: `DownloadVerifyTxResultsWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `DownloadVerifyTxResultsWork(...)` | `--` | None |
| `getStatus()` | `HistoryWorkProgress` | Partial |

Component status: None - there is no Rust work item that walks a checkpoint range and spawns download+verify steps equivalent to stellar-core's batch verifier.

### PutHistoryArchiveStateWork (`lib.rs`)

Corresponds to: `PutHistoryArchiveStateWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PutHistoryArchiveStateWork(...)` | `PublishHistoryArchiveStateWork::new(...)` | Full |

### Progress (`lib.rs`)

Corresponds to: `Progress.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `fmtProgress(...)` | `HistoryWorkProgress` / `BatchDownloadProgress::message()` | Full |

### WriteSnapshotWork (`lib.rs`)

Corresponds to: `WriteSnapshotWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `WriteSnapshotWork(...)` | `--` | None |

### ResolveSnapshotWork (`lib.rs`)

Corresponds to: `ResolveSnapshotWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `ResolveSnapshotWork(...)` | `--` | None |

### PutFilesWork (`lib.rs`)

Corresponds to: `PutFilesWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PutFilesWork(...)` | `--` | None |

### PutSnapshotFilesWork (`lib.rs`)

Corresponds to: `PutSnapshotFilesWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PutSnapshotFilesWork(...)` | `--` | None |

### PublishWork (`lib.rs`)

Corresponds to: `PublishWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PublishWork(...)` | `--` | None |

### WriteVerifiedCheckpointHashesWork (`lib.rs`)

Corresponds to: `WriteVerifiedCheckpointHashesWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `WriteVerifiedCheckpointHashesWork(...)` | `--` | None |
| `loadHashFromJsonOutput(...)` | `--` | None |
| `loadLatestHashPairFromJsonOutput(...)` | `--` | None |

### FetchRecentQsetsWork (`lib.rs`)

Corresponds to: `FetchRecentQsetsWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `FetchRecentQsetsWork(...)` | `--` | None |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `RunCommandWork` | Rust uses native async libraries instead of subprocess-driven shell helpers |
| `GetRemoteFileWork` | HTTP download is handled directly by `henyey-history` with `reqwest` |
| `GetAndUnzipRemoteFileWork` | Download + decompress is handled natively, not as chained shell work |
| `GunzipFileWork` | Decompression uses `flate2` rather than `gunzip` subprocesses |
| `GzipFileWork` | Compression uses `flate2` rather than `gzip` subprocesses |
| `PutRemoteFileWork` | Upload is abstracted behind the `ArchiveWriter` trait |
| `MakeRemoteDirWork` | Archive-writer implementations create directories implicitly |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `DownloadBucketsWork` | Medium | Missing bucket-manager adoption and persistent bucket index ownership |
| `VerifyBucketWork` | Medium | No standalone verifier that builds indexes in the background |
| `DownloadVerifyTxResultsWork` | Medium | No checkpoint-range result download+verify orchestration |
| `WriteSnapshotWork` | Medium | Live-state snapshot writer for publish workflow is absent |
| `ResolveSnapshotWork` | Medium | Snapshot bucket-reference resolution is absent |
| `PutFilesWork` | Medium | Differential upload against remote HAS is not implemented |
| `PutSnapshotFilesWork` | Medium | Snapshot gzip/upload pipeline is missing |
| `PublishWork` | Medium | Top-level publish orchestration and callbacks are absent |
| `WriteVerifiedCheckpointHashesWork` | Low | Offline verified hash-chain export is missing |
| `FetchRecentQsetsWork` | Low | Recent SCP qset bootstrap helper is missing |

## Architectural Differences

1. **Transport and compression**
   - **stellar-core**: Builds many history tasks out of shell-command work items (`curl`, `gzip`, `gunzip`).
   - **Rust**: Uses native HTTP and compression libraries directly inside async work items.
   - **Rationale**: Avoids subprocess overhead and fits the tokio-based scheduler.

2. **Work scheduling**
   - **stellar-core**: Uses `Work`/`BatchWork` state machines with child-work spawning.
   - **Rust**: Registers explicit DAG dependencies through `henyey-work` builders.
   - **Rationale**: Makes dependency ordering explicit and easier to compose.

3. **Bucket handling**
   - **stellar-core**: Verifies buckets and builds indexes before adopting them into `BucketManager`.
   - **Rust**: Downloads verified bucket files to disk and records only the bucket directory.
   - **Rationale**: Current catchup path only needs verified files, not full bucket-manager integration.

4. **Publish model**
   - **stellar-core**: Publishes from a `StateSnapshot` through snapshot-resolution and differential-upload work.
   - **Rust**: Publishes already-downloaded checkpoint artifacts through `ArchiveWriter`.
   - **Rationale**: Current Rust code supports archive mirroring, not a full archiving-node publish pipeline.

5. **Tx-result verification scope**
   - **stellar-core**: Exposes a dedicated batch work item for download+verify over checkpoint ranges.
   - **Rust**: Verifies tx-result files only in the single-checkpoint download path.
   - **Rationale**: Sufficient for current catchup flows, but not parity for offline range verification.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Verified checkpoint hashes | 1 TEST_CASE / 2 SECTION | 0 `#[test]` | Feature not implemented in Rust |
| Single ledger header check | 1 TEST_CASE / 0 SECTION | 0 direct `#[test]` | No dedicated regression test for `CheckSingleLedgerHeaderWork` |
| Download + publish pipeline | Indirect via `CatchupSimulation` | 1 `#[tokio::test]` | `history_work.rs` exercises end-to-end fetch and mirror publish |
| Checkpoint data assembly | -- | 2 `#[tokio::test]` | Covers `build_checkpoint_data()` success and failure paths |
| Helper types and progress | -- | 8 `#[test]` | Covers ranges, file-type strings, progress, constants, well-known path |

### Test Gaps

- `WriteVerifiedCheckpointHashesWork` has upstream acceptance coverage and no Rust equivalent.
- `CheckSingleLedgerHeaderWork` lacks a dedicated Rust test despite upstream coverage.
- Snapshot publishing and qset bootstrap have no Rust implementation or tests.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 6 |
| Gaps (None + Partial) | 10 |
| Intentional Omissions | 7 |
| **Parity** | **6 / (6 + 10) = 38%** |
