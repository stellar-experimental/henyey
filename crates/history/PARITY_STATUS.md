# stellar-core Parity Status

**Crate**: `henyey-history`
**Upstream**: `stellar-core/src/history/`
**Overall Parity**: 79%
**Last Updated**: 2026-06-16

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Checkpoint math | Full | Matches 64-ledger checkpoint rules |
| Archive path generation | Full | Standard shard and dirty paths covered |
| HAS parsing and diffing | Full | Bucket-list hash, parsing, futures, and diffing all covered |
| Archive HTTP and shell access | Full | Native HTTP reads plus shell-command writes; offline publish→catchup round-trip over a `cp`/`mkdir` command-template archive and `file://` is CI-pinned (`tests/publish_catchup_roundtrip.rs`, #3295) |
| Archive manager | Partial | No random selection or work wrappers |
| Checkpoint builder | Full | Crash-safe dirty-file recovery implemented |
| Publish queue persistence | Full | SQLite-backed queue replaces filesystem queue |
| Publish orchestration | Partial | Missing some callbacks, metrics, cleanup helpers |
| State snapshot publishing | Partial | No differential HAS upload; upload ordering now matches stellar-core (data-before-HAS) |
| Verification | Full | Header entry hash, header chain, bucket, tx-set, tx-result checks implemented; SCP trust anchor plumbed from online catchup (§9.1 + INV-C5) |
| Catchup and replay | Full | Rust-native orchestration covers core flow; §11.2 5-case knit-to-LCL decision matrix matches stellar-core |
| Metrics and status plumbing | None | No Medida/StatusManager equivalent yet |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `HistoryManager.h` | `checkpoint.rs`, `publish_queue.rs`, `publish.rs`, `lib.rs` | Static checkpoint helpers and publish APIs are split across modules |
| `HistoryManagerImpl.h` | `publish.rs`, `publish_queue.rs`, `checkpoint_builder.rs` | Rust keeps publish state in plain structs instead of one manager impl |
| `HistoryArchive.h` | `archive_state.rs`, `archive.rs`, `remote_archive.rs` | HAS state and archive transport are split |
| `HistoryArchiveManager.h` | `lib.rs` | Archive registry and initialization live at crate root |
| `FileTransferInfo.h` | `paths.rs` | Path generation is function-based rather than object-based |
| `CheckpointBuilder.h` | `checkpoint_builder.rs` | Crash-safe stream building maps closely |
| `StateSnapshot.h` | `publish.rs` | PublishManager replaces `StateSnapshot` |
| `HistoryUtils.h` | `replay.rs` | Gap-tolerant history iteration is inlined into replay logic |
| `src/catchup/` and `src/historywork/` work classes | `catchup/*.rs`, `download.rs`, `verify.rs` | Rust crate also absorbs catchup/download orchestration beyond `src/history/` |

## Component Mapping

### checkpoint.rs / paths.rs (`checkpoint.rs`, `paths.rs`)

Corresponds to: `HistoryManager.h`, `FileTransferInfo.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `getCheckpointFrequency()` | `checkpoint_frequency()` / `CHECKPOINT_FREQUENCY` | Full |
| `checkpointContainingLedger()` | `checkpoint_containing()` / `checkpoint_ledger()` | Full |
| `publishCheckpointOnLedgerClose()` | `is_checkpoint_ledger()` | Full |
| `isFirstLedgerInCheckpoint()` | `first_ledger_in_checkpoint_containing()` | Full |
| `isLastLedgerInCheckpoint()` | `is_checkpoint_ledger()` | Full |
| `sizeOfCheckpointContaining()` | `size_of_checkpoint_containing()` | Full |
| `firstLedgerInCheckpointContaining()` | `first_ledger_in_checkpoint_containing()` | Full |
| `firstLedgerAfterCheckpointContaining()` | `next_checkpoint()` | Full |
| `lastLedgerBeforeCheckpointContaining()` | `last_ledger_before_checkpoint_containing()` | Full |
| `ledgerToTriggerCatchup()` | `ledger_to_trigger_catchup()` | Full |
| `FileType` / `typeString()` | category strings in `paths.rs` | Full |
| `createPath()` | `std::fs::create_dir_all()` call sites | Full |
| `createPublishDir()` / `getPublishHistoryDir()` | `checkpoint_path*()` helpers plus publish dir management | Full |
| `localPath_nogz*()` / `remoteDir()` / `remoteName()` | `checkpoint_path()`, `checkpoint_path_dirty()`, `bucket_path()`, `has_path()` | Full |

### archive_state.rs (`archive_state.rs`)

Corresponds to: `HistoryArchive.h` (`HistoryArchiveState`)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `HistoryArchiveState()` | `HistoryArchiveState` serde struct | Full |
| `HistoryArchiveState(ledgerSeq, ...)` | `build_history_archive_state()` | Full |
| `baseName()` | `root_has_path()` | Full |
| `wellKnownRemoteDir()` / `wellKnownRemoteName()` | `.well-known/stellar-history.json` helpers | Full |
| `remoteDir()` / `remoteName()` | `has_path()` / `checkpoint_path("history", ...)` | Full |
| `localName()` | -- | Omission |
| `getBucketListHash()` | `compute_bucket_list_hash()` | Full |
| `differingBuckets()` | `differing_bucket_hashes()` / `all_differing_bucket_hashes()` | Full |
| `allBuckets()` | `all_bucket_hashes()` / `unique_bucket_hashes()` | Full |
| `serialize()` / `deserialize()` | `to_json()` / `from_json()` | Full |
| `futuresAllClear()` | `futures_all_clear()` | Full |
| `futuresAllResolved()` | `futures_all_resolved()` | Full |
| `resolveAllFutures()` | `resolve_all_futures()` | Full |
| `resolveAnyReadyFutures()` | -- | None |
| `save()` / `load()` | `to_json()` / `from_json()` plus file I/O call sites | Full |
| `toString()` / `fromString()` | `to_json()` / `from_json()` | Full |
| `prepareForPublish()` | `build_history_archive_state()` | Partial |
| `containsValidBuckets()` | `contains_valid_buckets()` | Full |
| `hasHotArchiveBuckets()` | `has_hot_archive_buckets()` | Full |

### archive.rs / remote_archive.rs (`archive.rs`, `remote_archive.rs`)

Corresponds to: `HistoryArchive.h` (`HistoryArchive`)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `HistoryArchive(config)` | `HistoryArchive::new()` / `RemoteArchive::new()` | Full |
| `hasGetCmd()` | `can_read()` | Full |
| `hasPutCmd()` | `can_write()` | Full |
| `hasMkdirCmd()` | `config.mkdir_cmd.is_some()` | Full |
| `getName()` | `name()` / archive entry name | Full |
| `getFileCmd()` | `get_file()` / HTTP `get_*()` methods | Full |
| `putFileCmd()` | `put_file()` / `put_file_with_mkdir()` | Full |
| `mkdirCmd()` | `mkdir()` / `ensure_dir()` | Full |

### lib.rs (`lib.rs`)

Corresponds to: `HistoryArchiveManager.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `HistoryArchiveManager(app)` | `HistoryArchiveManager::new()` | Full |
| `checkSensibleConfig()` | `check_sensible_config()` | Full |
| `selectRandomReadableHistoryArchive()` | `get_readable_archives()` | Partial |
| `getHistoryArchiveReportWork()` | -- | None |
| `getCheckLedgerHeaderWork()` | -- | None |
| `initializeHistoryArchive()` | `initialize_history_archive()` | Full — writes both the §4.3 ledger-zero pseudo-checkpoint (`history/00/00/00/history-00000000.json`) and the well-known root HAS, matching `PutHistoryArchiveStateWork::spawnPublishWork`. |
| `publishEnabled()` | `publish_enabled()` | Full |
| `getHistoryArchive()` | `get_archive()` | Full |
| `getWritableHistoryArchives()` | `get_writable_archives()` | Full |

### checkpoint_builder.rs (`checkpoint_builder.rs`)

Corresponds to: `CheckpointBuilder.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `CheckpointBuilder(app)` | `CheckpointBuilder::new()` | Full |
| `appendTransactionSet()` | `append_transaction_set()` | Full |
| `appendLedgerHeader()` | `append_ledger_header()` | Full |
| `cleanup(lcl)` | `cleanup()` | Full |
| `checkpointComplete(checkpoint)` | `checkpoint_complete()` | Full |
| `ensureOpen(ledgerSeq)` | `ensure_open()` | Full |
| `skipIncompleteFirstCheckpointSinceRestart()` | publish-queue enqueue guard in `LedgerPersistInputs::serialize_and_write_to_db` (post-catchup branch only) | Partial |

> **Architectural note (#2681).** stellar-core hosts the
> `skipFirstCheckpointSinceItIsIncomplete` flag inside `CheckpointBuilder`
> because it writes `.dirty` per-ledger files and re-derives the flag at
> startup from "no dirty files on disk". Henyey doesn't have a per-ledger
> append path — checkpoint files are reconstructed from SQLite at publish
> time — so the parity equivalent is implemented one level up, at the
> publish-queue enqueue site. `CatchupPersistData::write_to_db` records
> the target checkpoint sequence in the `publishskipfirstcheckpoint`
> state key when catchup ends mid-checkpoint with publish enabled, and
> the next `LedgerPersistInputs::serialize_and_write_to_db` for that
> checkpoint suppresses its `enqueue_publish` and clears the marker — all
> inside one DB transaction.
>
> stellar-core's trigger is "no dirty files at startup", which covers two
> paths: (a) post-catchup, and (b) publish-disabled-then-re-enabled. Henyey
> currently implements only (a). Path (b) is not a supported henyey
> production configuration today (publish-disable on a validator has no
> precedent and no observable archive divergence as long as it stays
> disabled), so the gap is intentional. If/when path (b) becomes a
> supported configuration, the follow-up should derive the marker at
> startup from on-disk state (e.g., checkpoint files present but no
> queued publish for the corresponding ledger).

### publish_queue.rs / publish.rs (`publish_queue.rs`, `publish.rs`)

Corresponds to: `HistoryManager.h`, `HistoryManagerImpl.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `create()` | `PublishQueue::new()` / `PublishManager::new()` | Full |
| `publishQueuePath()` | SQLite table in `henyey-db` | Full |
| `publishQueueLength()` | `PublishQueue::len()` | Full |
| `logAndUpdatePublishStatus()` | `PublishQueue::log_status()` | Partial |
| `maybeQueueHistoryCheckpoint()` | handled in `henyey-app` | Omission |
| `queueCurrentHistory()` | `PublishQueue::enqueue()` | Full |
| `getMinLedgerQueuedToPublish()` | `PublishQueue::min_ledger()` | Full |
| `getMaxLedgerQueuedToPublish()` | `PublishQueue::max_ledger()` | Full |
| `publishQueuedHistory()` | -- | Partial |
| `maybeCheckpointComplete()` | `CheckpointBuilder::checkpoint_complete()` | Full |
| `getMissingBucketsReferencedByPublishQueue()` | -- | None |
| `getBucketsReferencedByPublishQueue()` | `get_referenced_bucket_hashes()` | Full |
| `getPublishQueueStates()` | `get_all()` / `get_state()` | Full |
| `historyPublished()` | -- | None |
| `appendTransactionSet()` | `CheckpointBuilder::append_transaction_set()` | Full |
| `appendLedgerHeader()` | `CheckpointBuilder::append_ledger_header()` | Full |
| `restoreCheckpoint()` | `cleanup()` / `finalize_recovered_checkpoint()` | Full |
| `deletePublishedFiles()` | -- | None |
| `getPublishQueueCount()` | `len()` / `stats()` | Full |
| `getPublishSuccessCount()` | -- | None |
| `getPublishFailureCount()` | -- | None |
| `waitForCheckpointPublish()` | -- | None |
| `getTmpDir()` | tempfile / path abstractions | Omission |
| `localFilename()` | direct path construction | Omission |

### publish.rs (`publish.rs`)

Corresponds to: `StateSnapshot.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `StateSnapshot(app, state)` | `PublishManager` plus `build_history_archive_state()` | Full |
| `writeSCPMessages()` | -- | None |
| `differingHASFiles()` | -- | None |
| `takeSnapshotAndPublish()` | `publish_checkpoint()` | Full |

### upload.rs (`upload.rs`)

Corresponds to: `PutSnapshotFilesWork.cpp`, `PutHistoryArchiveStateWork.cpp`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `PutSnapshotFilesWork` data-before-HAS ordering | `UploadPlan` with `UploadFileKind` enum | Full |
| `PutHistoryArchiveStateWork` permanent+`.well-known` upload | `CheckpointHas` → `RootHas` ordering | Full (intentional strengthening: sequential vs parallel) |
| `PutFilesWork` differential upload | -- | None (follow-up) |

### replay modules (`replay/*.rs`, `catchup/replay.rs`)

Corresponds to: `HistoryUtils.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `getHistoryEntryForLedger()` | inlined sparse-entry iteration in replay/catchup code | Partial |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `localName()` | Rust keeps HAS temp files in generic temp-file helpers rather than a named method |
| `maybeQueueHistoryCheckpoint()` | Checkpoint enqueue orchestration is owned by `henyey-app`, not this crate |
| `getTmpDir()` | Temporary directory management is delegated to `tempfile` and call-site paths |
| `localFilename()` | Rust uses direct `PathBuf` composition instead of a manager method |
| `createPublishQueueDir()` | Publish queue is SQLite-backed, so no filesystem queue directory exists |
| `dropSQLBasedPublish()` | Rust never had the legacy SQL publish migration path |
| `setPublicationEnabled()` / `getConfig()` | Test-only toggle and config getter are not exposed in the Rust API shape |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `resolveAnyReadyFutures()` | Low | Incremental FutureBucket resolution is still missing |
| `prepareForPublish()` parity | Low | Publish preparation exists, but not as a full HAS instance method |
| `selectRandomReadableHistoryArchive()` | Low | Readable archives are returned, not randomly selected |
| `getHistoryArchiveReportWork()` | Low | No archive report diagnostic work wrapper |
| `getCheckLedgerHeaderWork()` | Low | No dedicated per-archive ledger-header work wrapper |
| `skipIncompleteFirstCheckpointSinceRestart()` | Low | Rust builder recovers files but has no exposed skip flag |
| `logAndUpdatePublishStatus()` parity | Low | Logging exists, but no StatusManager state tracking |
| `publishQueuedHistory()` | Medium | No full work-scheduler style publish loop |
| `getMissingBucketsReferencedByPublishQueue()` | Medium | Queue cannot yet report unresolved bucket dependencies |
| `historyPublished()` | Medium | No completion callback to dequeue/retry based on all-archive success |
| `deletePublishedFiles()` | Low | Published file cleanup helper is absent |
| `getPublishSuccessCount()` | Low | Success counters are not tracked |
| `getPublishFailureCount()` | Low | Failure counters are not tracked |
| `waitForCheckpointPublish()` | Low | No blocking wait helper for utility flows |
| `writeSCPMessages()` | Low | Publish path still omits SCP history snapshot files |
| `differingHASFiles()` | Low | No differential upload optimization for publish state |
| `getHistoryEntryForLedger()` helper | Low | Logic exists inline, not as a reusable utility function |

## Architectural Differences

1. **Catchup orchestration**
   - **stellar-core**: Uses `BasicWork` graphs spread across `src/history/`, `src/catchup/`, and `src/historywork/`.
   - **Rust**: Uses `CatchupManager` plus async helper modules for download, verification, and replay.
   - **Rationale**: The Rust crate folds historywork-style scheduling into one async pipeline.

2. **Archive transport split**
   - **stellar-core**: Uses shell-command templates for both read and write archive access.
   - **Rust**: Uses native HTTP (`reqwest`) for reads and shell commands for write-compatible remotes.
   - **Rationale**: Native HTTP improves retries, error reporting, and connection reuse on the common read path.

3. **Publish queue storage**
   - **stellar-core**: Uses a filesystem queue directory where each pending checkpoint is represented by `<seq>.checkpoint.dirty` (written by `writeCheckpointFile(..., false)` in `publishQueuePath()`) and finalized to `<seq>.checkpoint` by `maybeCheckpointComplete()`. On restart, `restoreCheckpoint()` iterates the queue directory and removes entries above LCL.
   - **Rust**: Uses a SQLite `publishqueue` table (`ledgerseq → HAS JSON`). Rows are created atomically inside the ledger-close DB transaction (`conn.enqueue_publish(...)` in `crates/app/src/app/ledger_close.rs`). There is no pending/dirty row state — the entry either committed or it didn't. On restart, `restore_checkpoint()` calls `db.remove_publish_above_lcl(lcl)` to clean stale rows, paralleling stellar-core's filesystem cleanup.
   - **Rationale**: The queue is node-local and never externally observable. SQLite's ACID commit replaces the two-phase `.dirty` → rename pattern while providing equivalent durability and restart-safety guarantees. The Rust implementation skips migration baggage and keeps queue state crash-safe in one transactional store. Checkpoint XDR file staging (the part that *is* filesystem-based) remains in `CheckpointBuilder` with `.dirty` files and durable rename, identical in spirit to stellar-core.

4. **FutureBucket handling**
   - **stellar-core**: Exposes both blocking and incremental future-resolution helpers on HAS.
   - **Rust**: Implements state inspection and full resolution, but not incremental ready-only resolution.
   - **Rationale**: Rust currently only needs full replay/publish settlement paths.

5. **Publishing model**
   - **stellar-core**: Uses `StateSnapshot`, publish callbacks, status updates, and Medida counters.
   - **Rust**: Uses `PublishManager`, `PublishQueue`, and structured logs without a scheduler callback layer.
   - **Rationale**: Core file generation is implemented first; operational polish is still pending.

6. **~~CatchupRange Case 1: replay-budget optimization for Recent(N) and Minimal~~** *(RESOLVED)*
   - **stellar-core**: Case 1 (`lcl > genesis`) unconditionally replays from `lcl+1` to target regardless of mode or count (`CatchupRange.cpp:52-57`).
   - **Rust**: Now matches stellar-core exactly — Case 1 unconditionally replays from `lcl+1` when `lcl > genesis`. The per-mode replay-budget optimization was removed to enforce INV-C15 (no bucket-apply older than LCL). See #2677.
   - **Status**: Parity achieved.

7. **Empty tx set synthesis uses LCL protocol version**
   - **stellar-core**: `TxSetXDRFrame::makeEmpty(lclHeader)` (`TxSetFrame.cpp:958-979`) uses `lclHeader.header.ledgerVersion` to choose between Classic and Generalized format.
   - **Rust**: `make_empty_tx_set(previous_ledger_hash, lcl_protocol_version)` mirrors this — branches on the LCL's protocol, not the current header's.
   - **Parity fix**: Previously used the current ledger's `ledger_version`, which broke at protocol upgrade boundaries (e.g., ledger 2 of quickstart). Fixed in #2279.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Checkpoint math and paths | 1 `TEST_CASE` / 0 `SECTION` | 23 `#[test]` | Rust covers math and dirty-path helpers thoroughly |
| HAS serialization and bucket state | 2 `TEST_CASE` / 2 `SECTION` | 31 `#[test]` | Strong Rust coverage for parsing, futures, and diffing |
| Bucket and chain verification | 3 `TEST_CASE` / 31 `SECTION` | 35 `#[test]` | Rust has good unit coverage but fewer archive-failure integration cases |
| Archive manager and transport | 4 `TEST_CASE` / 5 `SECTION` | 32 `#[test]` | Rust adds native HTTP and shell-command coverage |
| Checkpoint builder and queue | 2 `TEST_CASE` / 6 `SECTION` | 25 `#[test]` | Rust covers recovery paths well |
| Publish workflows | 6 `TEST_CASE` / 18 `SECTION` | 15 `#[test]` | Missing restart, multi-archive, and throttling scenarios |
| Catchup and replay | 17 `TEST_CASE` / 9 `SECTION` | 76 `#[test]` | Rust has broad unit coverage but lighter end-to-end catchup scenarios |
| CDP, compare, and error utilities | No dedicated upstream tests | 8 `#[test]` | Rust-native helpers outside direct history file mapping |
| **Total** | **37 `TEST_CASE` / 71 `SECTION`** | **245 `#[test]`** | Upstream still has more scenario-heavy acceptance coverage |

### Test Gaps

- Bucket download failure modes are thinner on the Rust side for truncated gzip, invalid enum, and other corrupt-archive scenarios exercised upstream.
- Ledger-chain verification lacks stellar-core-style coverage for bad ledger version, overshot, undershot, and missing-entry status variants.
- Publish flow coverage does not yet match upstream restart, multi-archive, post-shadow-removal, and throttled catchup scenarios.
- Catchup coverage is still lighter for buffered-ledger recovery, gap handling, protocol-upgrade catchup, and fatal-failure recovery paths.
- CheckpointBuilder has strong recovery tests, but not the exact upstream crash-injection hooks used by `mThrowOnAppend`.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 65 |
| Gaps (None + Partial) | 17 |
| Intentional Omissions | 7 |
| **Parity** | **65 / (65 + 17) = 79%** |
