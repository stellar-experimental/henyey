# stellar-core Parity Status

**Crate**: `henyey-history`
**Upstream**: `stellar-core/src/history/`
**Overall Parity**: 82%
**Last Updated**: 2026-03-05

## Summary

| Area | Status | Notes |
|------|--------|-------|
| Checkpoint arithmetic | Full | All static methods implemented |
| History Archive State (HAS) | Full | FutureBucket resolution, differing buckets, validation |
| HistoryArchive (shell commands) | Full | get/put/mkdir commands |
| HistoryArchiveManager | Partial | Missing report/check work |
| Publish queue | Full | SQLite-backed, crash-safe |
| CheckpointBuilder | Full | ACID dirty-file pattern |
| FileTransferInfo / paths | Full | All path generation covered |
| StateSnapshot / publish | Partial | Missing SCP messages |
| Publish metrics / callbacks | None | No Medida equivalent |
| Verification | Full | Header chain, bucket, tx set |
| Catchup orchestration | Full | 7-step process (Rust-native) |
| Ledger replay | Full | Re-execution and meta-based |

## File Mapping

| stellar-core File | Rust Module | Notes |
|--------------------|-------------|-------|
| `HistoryManager.h` / `HistoryManagerImpl.h` | `lib.rs`, `checkpoint.rs`, `publish_queue.rs` | Split across multiple modules |
| `HistoryArchive.h` (HistoryArchiveState) | `archive_state.rs` | JSON-based serde vs cereal |
| `HistoryArchive.h` (HistoryArchive class) | `remote_archive.rs` | Shell command archive ops |
| `HistoryArchiveManager.h` | `lib.rs` (HistoryArchiveManager) | Archive management |
| `FileTransferInfo.h` | `paths.rs` | Path generation functions |
| `CheckpointBuilder.h` | `checkpoint_builder.rs` | Crash-safe checkpoint building |
| `StateSnapshot.h` | `publish.rs` | Publishing workflow |
| `HistoryUtils.h` | Inline in `replay.rs` / `catchup.rs` | Entry iteration |

## Component Mapping

### checkpoint.rs / paths.rs (`checkpoint.rs`, `paths.rs`)

Corresponds to: `HistoryManager.h` (static checkpoint methods), `FileTransferInfo.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `getCheckpointFrequency()` | `CHECKPOINT_FREQUENCY` | Full |
| `checkpointContainingLedger()` | `checkpoint_ledger()` | Full |
| `publishCheckpointOnLedgerClose()` | `is_checkpoint_ledger()` | Full |
| `isFirstLedgerInCheckpoint()` | Compare with `first_ledger_in_checkpoint_containing()` | Full |
| `isLastLedgerInCheckpoint()` | `is_checkpoint_ledger()` | Full |
| `sizeOfCheckpointContaining()` | `size_of_checkpoint_containing()` | Full |
| `firstLedgerInCheckpointContaining()` | `first_ledger_in_checkpoint_containing()` | Full |
| `firstLedgerAfterCheckpointContaining()` | `next_checkpoint()` | Full |
| `lastLedgerBeforeCheckpointContaining()` | `last_ledger_before_checkpoint_containing()` | Full |
| `ledgerToTriggerCatchup()` | `ledger_to_trigger_catchup()` | Full |
| `FileType` enum | Category strings in `paths.rs` | Full |
| `typeString()` | Inline category strings | Full |
| `createPath()` | `fs::create_dir_all` | Full |
| `createPublishDir()` | `PublishManager` directory creation | Full |
| `getPublishHistoryDir()` | `PublishManager` path handling | Full |
| `FileTransferInfo` constructors | `checkpoint_path()`, `bucket_path()` | Full |
| `localPath_nogz()` / `localPath_nogz_dirty()` | `checkpoint_path()` / `checkpoint_path_dirty()` | Full |
| `localPath_gz()` / `localPath_gz_tmp()` | `checkpoint_path()` with `.gz` extension | Full |
| `baseName_nogz()` / `baseName_gz()` | `checkpoint_path()` basename extraction | Full |
| `remoteDir()` / `remoteName()` | `checkpoint_path()` URL generation | Full |
| `checkpoint_path()` dirty helpers | `checkpoint_path_dirty()`, `is_dirty_path()`, `dirty_to_final_path()`, `final_to_dirty_path()` | Full |

### archive_state.rs (`archive_state.rs`)

Corresponds to: `HistoryArchive.h` (HistoryArchiveState)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `HistoryArchiveState()` (default) | `HistoryArchiveState` struct with serde defaults | Full |
| `HistoryArchiveState(ledgerSeq, buckets, passphrase)` | `build_history_archive_state()` | Full |
| `baseName()` | `root_has_path()` | Full |
| `wellKnownRemoteDir()` | `.well-known/` in paths | Full |
| `wellKnownRemoteName()` | `root_has_path()` | Full |
| `remoteDir()` | `checkpoint_path("history", ...)` | Full |
| `remoteName()` | `has_path()` | Full |
| `getBucketListHash()` | `get_bucket_list_hash()` | Full |
| `differingBuckets()` | `differing_bucket_hashes()` | Full |
| `allBuckets()` | `all_bucket_hashes()` | Full |
| `serialize()` / `deserialize()` | `from_json()` / `to_json()` via serde | Full |
| `futuresAllClear()` | `futures_all_clear()` | Full |
| `futuresAllResolved()` | `futures_all_resolved()` | Full |
| `resolveAllFutures()` | `resolve_all_futures()` | Full |
| `resolveAnyReadyFutures()` | -- | None |
| `save()` / `load()` | `to_json()` / `from_json()` + file I/O | Full |
| `toString()` / `fromString()` | `to_json()` / `from_json()` | Full |
| `prepareForPublish()` | `build_history_archive_state()` | Full |
| `containsValidBuckets()` | `contains_valid_buckets()` | Full |
| `hasHotArchiveBuckets()` | `has_hot_archive_buckets()` | Full |

### remote_archive.rs (`remote_archive.rs`)

Corresponds to: `HistoryArchive.h` (HistoryArchive class)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `HistoryArchive(config)` | `RemoteArchive::new(config)` | Full |
| `hasGetCmd()` | `can_read()` | Full |
| `hasPutCmd()` | `can_write()` | Full |
| `hasMkdirCmd()` | `config.mkdir_cmd.is_some()` | Full |
| `getName()` | `name()` | Full |
| `getFileCmd()` | `get_file()` | Full |
| `putFileCmd()` | `put_file()` | Full |
| `mkdirCmd()` | `mkdir()` | Full |

### lib.rs - HistoryArchiveManager (`lib.rs`)

Corresponds to: `HistoryArchiveManager.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `HistoryArchiveManager(app)` | `HistoryArchiveManager::new(passphrase)` | Full |
| `checkSensibleConfig()` | `check_sensible_config()` | Full |
| `selectRandomReadableHistoryArchive()` | `get_readable_archives()` | Partial |
| `getHistoryArchiveReportWork()` | -- | None |
| `getCheckLedgerHeaderWork()` | -- | None |
| `initializeHistoryArchive()` | `initialize_history_archive()` | Full |
| `publishEnabled()` | `publish_enabled()` | Full |
| `getHistoryArchive()` | `get_archive()` | Full |
| `getWritableHistoryArchives()` | `get_writable_archives()` | Full |

### checkpoint_builder.rs (`checkpoint_builder.rs`)

Corresponds to: `CheckpointBuilder.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `CheckpointBuilder(app)` | `CheckpointBuilder::new(publish_dir)` | Full |
| `appendTransactionSet()` (TxSetXDRFrame) | `append_transaction_set()` | Full |
| `appendTransactionSet()` (TransactionHistoryEntry) | `append_transaction_set()` | Full |
| `appendLedgerHeader()` | `append_ledger_header()` | Full |
| `cleanup(lcl)` | `cleanup(lcl)` | Full |
| `checkpointComplete(checkpoint)` | `checkpoint_complete(checkpoint)` | Full |
| `ensureOpen(ledgerSeq)` | `ensure_open(checkpoint)` | Full |

### publish_queue.rs / lib.rs - HistoryManager (`publish_queue.rs`, `lib.rs`)

Corresponds to: `HistoryManager.h` (publish queue methods), `HistoryManagerImpl.h`

| stellar-core | Rust | Status |
|--------------|------|--------|
| `checkSensibleConfig()` | `HistoryArchiveManager::check_sensible_config()` | Full |
| `create()` | `HistoryManager::new()` / `HistoryArchiveManager::new()` | Full |
| `dropAll()` | PublishQueue DB schema in henyey-db | Full |
| `publishQueuePath()` | DB-based (in-database) | Full |
| `publishQueueLength()` | `PublishQueue::len()` | Full |
| `logAndUpdatePublishStatus()` | `PublishQueue::log_status()` | Partial |
| `maybeQueueHistoryCheckpoint()` | Handled in app crate | Partial |
| `queueCurrentHistory()` | `PublishQueue::enqueue()` | Full |
| `getMinLedgerQueuedToPublish()` | `PublishQueue::min_ledger()` | Full |
| `getMaxLedgerQueuedToPublish()` | `PublishQueue::max_ledger()` | Full |
| `publishQueuedHistory()` | -- | Partial |
| `maybeCheckpointComplete()` | `CheckpointBuilder::checkpoint_complete()` | Full |
| `getMissingBucketsReferencedByPublishQueue()` | -- | None |
| `getBucketsReferencedByPublishQueue()` | `PublishQueue::get_referenced_bucket_hashes()` | Full |
| `getPublishQueueStates()` | `PublishQueue::get_all()` | Full |
| `historyPublished()` | -- | None |
| `appendTransactionSet()` | `CheckpointBuilder::append_transaction_set()` | Full |
| `appendLedgerHeader()` | `CheckpointBuilder::append_ledger_header()` | Full |
| `restoreCheckpoint()` | `CheckpointBuilder::cleanup()` | Full |
| `deletePublishedFiles()` | -- | None |
| `getPublishQueueCount()` | `PublishQueue::len()` | Full |
| `getPublishSuccessCount()` | -- | None |
| `getPublishFailureCount()` | -- | None |
| `waitForCheckpointPublish()` | -- | None |

### publish.rs (`publish.rs`)

Corresponds to: `StateSnapshot.h`, `HistoryManagerImpl.h` (publish methods)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `StateSnapshot(app, state)` | `PublishManager` construction | Full |
| `writeSCPMessages()` | -- | None |
| `differingHASFiles()` | -- | None |
| `takeSnapshotAndPublish()` | `PublishManager::publish_checkpoint()` | Full |

### catchup.rs / replay.rs (`catchup.rs`, `replay.rs`)

Corresponds to: Catchup workflow (spans multiple upstream Work classes)

| stellar-core | Rust | Status |
|--------------|------|--------|
| `getHistoryEntryForLedger()` | Inline iteration logic | Partial |

## Intentional Omissions

Features excluded by design. These are NOT counted against parity %.

| stellar-core Component | Reason |
|------------------------|--------|
| `createPublishQueueDir()` | Publish queue is SQLite-backed, no filesystem directory needed |
| `dropSQLBasedPublish()` | Never had SQL-based publish format to migrate from |
| `getTmpDir()` | Handled by Rust temp directory abstractions (`tempfile` crate) |
| `localFilename()` | Handled by Rust path manipulation directly |
| `setPublicationEnabled()` | Test-only method (`#ifdef BUILD_TESTS`) |
| `getConfig()` | Config passed at construction, no getter needed |
| `localName()` (HAS) | Handled differently via temp file abstractions |

## Gaps

Features not yet implemented. These ARE counted against parity %.

| stellar-core Component | Priority | Notes |
|------------------------|----------|-------|
| `logAndUpdatePublishStatus()` | Low | Missing StatusManager integration (logs only) |
| `maybeQueueHistoryCheckpoint()` | Low | Orchestration in app crate, not history crate |
| `publishQueuedHistory()` | Medium | No work-based publish orchestration |
| `getMissingBucketsReferencedByPublishQueue()` | Medium | Missing bucket availability check |
| `historyPublished()` | Medium | No publish completion callback |
| `deletePublishedFiles()` | Low | Published file cleanup |
| `getPublishSuccessCount()` | Low | Metrics tracking |
| `getPublishFailureCount()` | Low | Metrics tracking |
| `waitForCheckpointPublish()` | Low | Blocking publish wait |
| `resolveAnyReadyFutures()` | Low | Incremental FutureBucket resolution |
| `writeSCPMessages()` | Low | SCP envelope publishing |
| `differingHASFiles()` | Low | Differential file upload optimization |
| `selectRandomReadableHistoryArchive()` | Low | Random selection (sequential used instead) |
| `getHistoryArchiveReportWork()` | Low | Archive reporting diagnostics |
| `getCheckLedgerHeaderWork()` | Low | Ledger header verification work |
| `getHistoryEntryForLedger()` | Low | Handled inline, not as separate utility |

## Architectural Differences

1. **Async model**
   - **stellar-core**: Work-based state machine (`WorkScheduler`, `BasicWork` subclasses). Catchup is orchestrated as a graph of dependent Work objects.
   - **Rust**: `async/await` with Tokio. Catchup is a single `CatchupManager` with sequential async steps.
   - **Rationale**: Rust's native async is more idiomatic and avoids the complexity of manual state machines.

2. **Publish queue storage**
   - **stellar-core**: Originally filesystem-based, migrated to SQL with `dropSQLBasedPublish()`. Uses file-based checkpoint builder plus DB queue.
   - **Rust**: SQLite-backed from the start via `henyey-db`. No filesystem queue migration needed.
   - **Rationale**: Database-backed queue is simpler and already crash-safe without extra filesystem coordination.

3. **FutureBucket handling**
   - **stellar-core**: Full FutureBucket resolution with in-progress merge tracking. HAS contains merge state that can be resolved on load.
   - **Rust**: Implements `futures_all_clear()`, `futures_all_resolved()`, `resolve_all_futures()`, `contains_valid_buckets()`, and `differing_bucket_hashes()`. Only `resolveAnyReadyFutures()` (incremental resolution of in-progress merges) remains unimplemented.
   - **Rationale**: Incremental merge resolution is tightly coupled to BucketManager's merge engine. The Rust side handles all state queries and batch resolution.

4. **HTTP archive access**
   - **stellar-core**: Shell command-based (`get` command templates) for all archive access. No built-in HTTP client.
   - **Rust**: Native HTTP client (`reqwest`) for reading archives, plus shell commands (`RemoteArchive`) for writing. Both approaches coexist.
   - **Rationale**: Native HTTP provides better error handling, retry logic, and connection pooling for the common read path.

5. **CDP data lake integration**
   - **stellar-core**: No CDP support; relies solely on traditional history archives.
   - **Rust**: First-class `CdpDataLake` and `CachedCdpDataLake` for accessing LedgerCloseMeta from SEP-0054 cloud storage.
   - **Rationale**: Enables direct access to transaction metadata for exact replay without full archive downloads.

6. **Metrics**
   - **stellar-core**: Medida-based metrics for publish success/failure counts, queue depth, publish latency.
   - **Rust**: Structured logging via `tracing` only. No numeric metric counters.
   - **Rationale**: Metrics infrastructure not yet built; `tracing` provides observability for now.

## Test Coverage

| Area | stellar-core Tests | Rust Tests | Notes |
|------|-------------------|------------|-------|
| Checkpoint arithmetic | 2 TEST_CASE / 3 SECTION | 21 `#[test]` | More thorough in Rust |
| HAS serialization | 1 TEST_CASE / 1 SECTION | 8 `#[test]` | Rust includes roundtrip, hot archive |
| Bucket verification | 5 TEST_CASE / 8 SECTION | 7 `#[test]` | Rust missing failure mode tests |
| Ledger chain verification | 3 TEST_CASE / 5 SECTION | 6 `#[test]` | Rust missing overshot/undershot/version |
| Publish workflow | 6 TEST_CASE / 12 SECTION | 11 `#[test]` | Rust missing multi-archive, restart |
| Catchup modes | 8 TEST_CASE / 15 SECTION | 13 `#[test]` | Rust missing online catchup, gap handling |
| Checkpoint builder | 2 TEST_CASE / 5 SECTION | 5 `#[test]` | Rust missing crash simulation |
| Remote archive | -- | 12 `#[test]` | Rust-only (shell command testing) |
| CDP / download | -- | 7 `#[test]` | Rust-only extensions |
| Archive manager | -- | 10 `#[test]` | Rust-only unit tests |
| **Total** | **34 TEST_CASE / 60 SECTION** | **122 `#[test]`** | |

### Test Gaps

- **Bucket verification failure modes**: stellar-core tests file-not-found, corrupted zip, and hash mismatch scenarios. Rust lacks these negative test cases.
- **Ledger chain verification edge cases**: stellar-core tests `VERIFY_STATUS_ERR_BAD_LEDGER_VERSION`, `VERIFY_STATUS_ERR_OVERSHOT`, `VERIFY_STATUS_ERR_UNDERSHOT`, `VERIFY_STATUS_ERR_MISSING_ENTRIES`. Rust only tests basic chain validation and broken hashes.
- **Online catchup with buffering**: stellar-core has extensive tests for online catchup with buffered ledgers, gaps, out-of-order delivery, trimming `mSyncingLedgers`, and recovery. Rust tests are primarily offline catchup.
- **Publish integration**: stellar-core tests publish with restart, multiple archives, post-shadow-removal, and throttled catchup. Rust lacks these integration tests.
- **Protocol upgrade during catchup**: stellar-core tests catching up across protocol boundaries. Rust lacks these.
- **CheckpointBuilder crash simulation**: stellar-core uses `mThrowOnAppend` to simulate crashes during checkpoint building. Rust lacks equivalent crash injection tests.

## Parity Calculation

| Category | Count |
|----------|-------|
| Implemented (Full) | 74 |
| Gaps (None + Partial) | 16 |
| Intentional Omissions | 7 |
| **Parity** | **74 / (74 + 16) = 82%** |
