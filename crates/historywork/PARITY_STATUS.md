# stellar-core Parity Status

**Crate**: `henyey-historywork`
**Upstream**: `stellar-core/src/historywork/`
**Overall Parity**: 56%
**Last Updated**: 2026-03-05

## Summary

| Area | Status | Notes |
|------|--------|-------|
| HAS Fetch | Full | `GetHistoryArchiveStateWork` |
| Bucket Download + Verification | Full | Parallel download with hash verification |
| Batch File Download | Full | `BatchDownloadWork` for checkpoint ranges |
| Header/Tx/Result/SCP Download | Full | Dedicated work items per file type |
| Single Header Verification | Full | `CheckSingleLedgerHeaderWork` |
| Bucket Verification | Full | Inline in download (no index building) |
| Tx Results Verification | Full | Inline in download |
| HAS Publishing | Full | Checkpoint path + well-known path |
| Data Publishing (mirror) | Full | Publish downloaded data to archive |
| Snapshot Publish Pipeline | None | WriteSnapshot, Resolve, PutFiles, etc. |
| Offline Verified Hash Chain | None | `WriteVerifiedCheckpointHashesWork` |
| Bootstrap QSet Fetch | None | `FetchRecentQsetsWork` |
| Progress Reporting | Full | Stage enum + message |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `GetHistoryArchiveStateWork.h` / `.cpp` | `lib.rs` (`GetHistoryArchiveStateWork`) | Full parity |
| `DownloadBucketsWork.h` / `.cpp` | `lib.rs` (`DownloadBucketsWork`) | Simplified; no bucket indexing |
| `BatchDownloadWork.h` / `.cpp` | `lib.rs` (`BatchDownloadWork`) | Full parity |
| `CheckSingleLedgerHeaderWork.h` / `.cpp` | `lib.rs` (`CheckSingleLedgerHeaderWork`) | Full parity |
| `VerifyBucketWork.h` / `.cpp` | `lib.rs` (inline in `DownloadBucketsWork`) | Hash verification only, no index |
| `VerifyTxResultsWork.h` / `.cpp` | `lib.rs` (inline in `DownloadTxResultsWork`) | Inline verification |
| `DownloadVerifyTxResultsWork.h` / `.cpp` | `lib.rs` (`DownloadTxResultsWork`) | Combined download + verify |
| `PutHistoryArchiveStateWork.h` / `.cpp` | `lib.rs` (`PublishHistoryArchiveStateWork`) | Full parity |
| `Progress.h` / `.cpp` | `lib.rs` (`HistoryWorkProgress`, `BatchDownloadProgress`) | Simplified |
| `WriteSnapshotWork.h` / `.cpp` | -- | Not implemented |
| `ResolveSnapshotWork.h` / `.cpp` | -- | Not implemented |
| `PutFilesWork.h` / `.cpp` | -- | Not implemented |
| `PutSnapshotFilesWork.h` / `.cpp` | -- | Not implemented |
| `PublishWork.h` / `.cpp` | -- | Not implemented |
| `WriteVerifiedCheckpointHashesWork.h` / `.cpp` | -- | Not implemented |
| `FetchRecentQsetsWork.h` / `.cpp` | -- | Not implemented |

## Component Mapping

### GetHistoryArchiveStateWork (`lib.rs`)

Corresponds to: `GetHistoryArchiveStateWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `GetHistoryArchiveStateWork()` constructor | `GetHistoryArchiveStateWork::new()` | Full |
| `getHistoryArchiveState()` | Via `SharedHistoryState.has` | Full |
| `getArchive()` | Field on struct | Full |
| `getStatus()` | `HistoryWorkProgress` stage reporting | Full |
| `doWork()` | `Work::run()` impl | Full |
| `doReset()` | Not needed (no child work) | Full |
| `onSuccess()` (metrics) | Not implemented | None |

### DownloadBucketsWork (`lib.rs`)

Corresponds to: `DownloadBucketsWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `DownloadBucketsWork()` constructor | `DownloadBucketsWork::new()` | Full |
| `getStatus()` | Progress reporting via `HistoryWorkStage` | Full |
| `hasNext()` | Implicit in `stream::iter` | Full |
| `yieldMoreWork()` | Implicit in `stream::iter` + `buffer_unordered` | Full |
| `resetIter()` | Not needed (single-pass async) | Full |
| `onSuccessCb()` (bucket adoption) | Not implemented (no BucketManager) | None |
| `prepareWorkForBucketType()` | Inline hash verification | Partial |
| Live bucket handling | `content_bucket_hashes()` | Full |
| Hot archive bucket handling | `content_bucket_hashes()` via HAS | Full |
| Bucket index building | Not implemented | None |

### BatchDownloadWork (`lib.rs`)

Corresponds to: `BatchDownloadWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `BatchDownloadWork()` constructor | `BatchDownloadWork::new()` | Full |
| `getStatus()` | `BatchDownloadWork::get_status()` | Full |
| `hasNext()` | Implicit in checkpoint iterator | Full |
| `yieldMoreWork()` | `download_checkpoint_file()` | Full |
| `resetIter()` | Not needed (single-pass async) | Full |

### CheckSingleLedgerHeaderWork (`lib.rs`)

Corresponds to: `CheckSingleLedgerHeaderWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `CheckSingleLedgerHeaderWork()` constructor | `CheckSingleLedgerHeaderWork::new()` | Full |
| `doWork()` | `Work::run()` impl | Full |
| `doReset()` | Not needed (no child work) | Full |
| `onFailureRaise()` (metrics) | Not implemented | None |
| `onSuccess()` (metrics) | Not implemented | None |

### VerifyBucketWork (inline in `DownloadBucketsWork`)

Corresponds to: `VerifyBucketWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `VerifyBucketWork()` constructor | Inline in download loop | Full |
| `onRun()` / `spawnVerifier()` | `verify::verify_bucket_hash()` | Full |
| `onFailureRaise()` | `WorkOutcome::Failed` | Full |
| Bucket index creation | Not implemented | None |

### VerifyTxResultsWork (inline in `DownloadTxResultsWork`)

Corresponds to: `VerifyTxResultsWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `VerifyTxResultsWork()` constructor | Inline in `DownloadTxResultsWork::run()` | Full |
| `onRun()` / `verifyTxResultsOfCheckpoint()` | `verify::verify_tx_result_set()` | Full |
| `getCurrentTxResultSet()` | Inline iteration over entries | Full |
| `onReset()` | Not needed | Full |

### DownloadVerifyTxResultsWork (combined in `DownloadTxResultsWork`)

Corresponds to: `DownloadVerifyTxResultsWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `DownloadVerifyTxResultsWork()` constructor | `DownloadTxResultsWork::new()` | Full |
| `getStatus()` | Progress reporting | Full |
| `hasNext()` | Not needed (single checkpoint) | Full |
| `yieldMoreWork()` | Combined in `run()` | Full |
| `resetIter()` | Not needed | Full |

### PutHistoryArchiveStateWork (`lib.rs`)

Corresponds to: `PutHistoryArchiveStateWork.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PutHistoryArchiveStateWork()` constructor | `PublishHistoryArchiveStateWork::new()` | Full |
| `doWork()` / `spawnPublishWork()` | `Work::run()` impl | Full |
| `doReset()` | Not needed | Full |
| Well-known path publish | Publishes to `.well-known/stellar-history.json` | Full |

### Progress (`lib.rs`)

Corresponds to: `Progress.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `fmtProgress()` | `HistoryWorkProgress` + `BatchDownloadProgress::message()` | Full |

### Publish Work Items (`lib.rs`)

These are Rust-specific work items with no direct 1:1 upstream equivalent. They publish downloaded data to an archive via the `ArchiveWriter` trait.

| Rust Work Item | Upstream Equivalent | Status |
|--------------|------|--------|
| `PublishBucketsWork` | Part of `PutSnapshotFilesWork` | Partial (mirror only) |
| `PublishLedgerHeadersWork` | Part of `PutSnapshotFilesWork` | Partial (mirror only) |
| `PublishTransactionsWork` | Part of `PutSnapshotFilesWork` | Partial (mirror only) |
| `PublishResultsWork` | Part of `PutSnapshotFilesWork` | Partial (mirror only) |
| `PublishScpHistoryWork` | Part of `PutSnapshotFilesWork` | Partial (mirror only) |

### Helper Functions and Types (`lib.rs`)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `CheckpointRange` (in `ledger/`) | `CheckpointRange` | Full |
| `FileType` (in `history/`) | `HistoryFileType` | Full |
| -- | `HistoryWorkBuilder` | Rust-specific builder |
| -- | `BatchDownloadWorkBuilder` | Rust-specific builder |
| -- | `HistoryWorkState` / `SharedHistoryState` | Rust-specific shared state |
| -- | `BatchDownloadState` / `SharedBatchDownloadState` | Rust-specific shared state |
| -- | `ArchiveWriter` trait / `LocalArchiveWriter` | Rust-specific abstraction |
| -- | `build_checkpoint_data()` | Rust-specific assembly |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `RunCommandWork` | Shell command execution base class; Rust uses native async libraries instead of spawning shell processes |
| `GetRemoteFileWork` | HTTP download via curl/wget shell commands; replaced by `reqwest` in `henyey-history` crate |
| `GetAndUnzipRemoteFileWork` | Download + gunzip via shell commands; replaced by async HTTP + `flate2` in-memory decompression |
| `GunzipFileWork` | Decompression via shell `gunzip`; replaced by `flate2` crate |
| `GzipFileWork` | Compression via shell `gzip`; replaced by `flate2` crate (`gzip_bytes()`) |
| `PutRemoteFileWork` | File upload via shell commands; replaced by `ArchiveWriter` trait abstraction |
| `MakeRemoteDirWork` | Remote directory creation via shell commands; handled by `ArchiveWriter` implementations |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `WriteSnapshotWork` | Medium | Writes current node state snapshot to disk for archival publishing; needed for archiving nodes |
| `ResolveSnapshotWork` | Medium | Resolves bucket references in a state snapshot; needed for archival publishing |
| `PutFilesWork` | Medium | Differential file upload (only uploads files not already in remote archive); needed for efficient archival publishing |
| `PutSnapshotFilesWork` | Medium | Orchestrates full snapshot publish pipeline (get remote state, gzip, upload); needed for archiving nodes |
| `PublishWork` | Medium | Top-level publish orchestration with success/failure callbacks to HistoryManager; needed for archiving nodes |
| `WriteVerifiedCheckpointHashesWork` | Low | Offline verification tool that downloads full ledger chain and writes verified hashes to JSON file |
| `FetchRecentQsetsWork` | Low | Downloads recent SCP history to extract quorum sets for network bootstrap |

## Architectural Differences

1. **File Downloads**
   - **stellar-core**: Shell commands (`curl`, `wget`) via `RunCommandWork` subprocess execution
   - **Rust**: Native async HTTP via `reqwest` in `henyey-history` crate
   - **Rationale**: Eliminates subprocess overhead and provides native async integration with tokio runtime

2. **Compression**
   - **stellar-core**: Shell commands (`gzip`, `gunzip`) via separate work items (`GzipFileWork`, `GunzipFileWork`)
   - **Rust**: In-memory compression/decompression via `flate2` crate
   - **Rationale**: Avoids file I/O round-trips and subprocess overhead; `gzip_bytes()` operates on byte slices directly

3. **Bucket Storage**
   - **stellar-core**: Files on disk with bucket indexing via `BucketManager.adoptFileAsBucket()`; `VerifyBucketWork` spawns background thread to verify and build index
   - **Rust**: Raw bucket files on disk (`<hash>.bucket.xdr` in configurable directory); hash verification inline during download, no index building
   - **Rationale**: Index building will be needed for full BucketManager parity but is not required for basic catchup

4. **Work Orchestration**
   - **stellar-core**: `BasicWork`/`Work`/`BatchWork` class hierarchy with state machine (`WORK_RUNNING`, `WORK_SUCCESS`, etc.) and child work management
   - **Rust**: `Work` trait with `WorkScheduler` DAG; dependencies expressed as work IDs; builders (`HistoryWorkBuilder`, `BatchDownloadWorkBuilder`) register work with proper ordering
   - **Rationale**: DAG-based scheduling is more explicit about dependencies and avoids state machine complexity

5. **Background Work**
   - **stellar-core**: `postOnBackgroundThread` for CPU-intensive tasks (bucket verification, snapshot writing)
   - **Rust**: All operations are async; no dedicated background threads
   - **Rationale**: tokio's task system provides equivalent concurrency without explicit thread management

6. **Publish Pipeline**
   - **stellar-core**: Multi-step snapshot pipeline: `WriteSnapshotWork` -> `ResolveSnapshotWork` -> `PutSnapshotFilesWork` (which does differential upload via `PutFilesWork`)
   - **Rust**: Individual publish work items (`PublishBucketsWork`, `PublishLedgerHeadersWork`, etc.) that publish from downloaded state via `ArchiveWriter` trait
   - **Rationale**: Current Rust implementation supports archive mirroring; full archiving-node publish from live state is not yet implemented

7. **Archive Selection**
   - **stellar-core**: Random archive selection on retry; `GetRemoteFileWork` picks a different archive each attempt
   - **Rust**: Single archive per work builder; retry uses same archive
   - **Rationale**: Simpler implementation; archive failover can be added at the `HistoryArchive` level

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Verified checkpoint hashes | 1 TEST_CASE / 2 SECTION | 0 #[test] | `WriteVerifiedCheckpointHashesWork` not implemented |
| Single ledger header check | 1 TEST_CASE / 0 SECTION | 0 #[test] | Upstream tests; no direct Rust equivalent but functionality is tested via integration |
| Checkpoint range | -- | 3 #[test] | `test_checkpoint_range_count`, `_iter`, `_ledger_range` |
| File type / progress | -- | 2 #[test] | `test_history_file_type_display`, `test_batch_download_progress_message` |
| Well-known path | -- | 1 #[test] | `test_well_known_stellar_history_path` |
| Integration (download) | Part of `CatchupSimulation` | 1 #[tokio::test] in `history_work.rs` | Rust has end-to-end test |
| State assembly | -- | 2 #[tokio::test] in `checkpoint_data.rs` | Basic state tests |

### Test Gaps

- **WriteVerifiedCheckpointHashesWork tests**: Upstream has 1 TEST_CASE with 2 SECTIONs for offline verification; functionality not implemented in Rust.
- **CheckSingleLedgerHeaderWork tests**: Upstream has 1 TEST_CASE; Rust has no direct unit test but the work item is exercised in integration tests.
- **CatchupSimulation-level tests**: stellar-core has elaborate simulation framework tests that exercise the full catchup pipeline including history work items; Rust has a simpler integration test.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 9 |
| Gaps (None + Partial) | 7 |
| Intentional Omissions | 7 |
| **Parity** | **9 / (9 + 7) = 56%** |

Implemented components: GetHistoryArchiveStateWork, DownloadBucketsWork, BatchDownloadWork, CheckSingleLedgerHeaderWork, VerifyBucketWork, VerifyTxResultsWork, DownloadVerifyTxResultsWork, PutHistoryArchiveStateWork, Progress.

Gap components: WriteSnapshotWork, ResolveSnapshotWork, PutFilesWork, PutSnapshotFilesWork, PublishWork, WriteVerifiedCheckpointHashesWork, FetchRecentQsetsWork.

Omitted components: RunCommandWork, GetRemoteFileWork, GetAndUnzipRemoteFileWork, GunzipFileWork, GzipFileWork, PutRemoteFileWork, MakeRemoteDirWork.
