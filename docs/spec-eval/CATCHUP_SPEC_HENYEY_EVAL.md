# Henyey Catchup/History Crates — Specification Adherence Evaluation

**Evaluated against:** `stellar-specs/CATCHUP_SPEC.md` (stellar-core v26.0.1 / Protocol 25)
**Crates:** `crates/history/` (henyey-history, 79% parity) and `crates/historywork/` (henyey-historywork, 49% parity)
**Date:** 2026-05-10

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [Evaluation Methodology](#2-evaluation-methodology)
3. [Section-by-Section Evaluation](#3-section-by-section-evaluation)
   - [§3 Data Types and Encoding](#31-data-types-and-encoding-spec-3)
   - [§4 History Archive Structure](#32-history-archive-structure-spec-4)
   - [§5 Checkpoint Publishing Pipeline](#33-checkpoint-publishing-pipeline-spec-5)
   - [§6 Catchup Configuration and Range Computation](#34-catchup-configuration-and-range-computation-spec-6)
   - [§7 Ledger Apply Manager](#35-ledger-apply-manager-spec-7)
   - [§8 Catchup Pipeline](#36-catchup-pipeline-spec-8)
   - [§9 Ledger Chain Verification](#37-ledger-chain-verification-spec-9)
   - [§10 Bucket Application](#38-bucket-application-spec-10)
   - [§11 Transaction Replay](#39-transaction-replay-spec-11)
   - [§12 Buffered Ledger Application](#310-buffered-ledger-application-spec-12)
   - [§13 Error Handling and Recovery](#311-error-handling-and-recovery-spec-13)
   - [§14 Invariants and Safety Properties](#312-invariants-and-safety-properties-spec-14)
   - [§15 Constants](#313-constants-spec-15)
4. [Gap Summary](#4-gap-summary)
5. [Risk Assessment](#5-risk-assessment)
6. [Recommendations](#6-recommendations)
7. [v26.0.1 Implementation Delta](#7-v2601-implementation-delta)

---

## 1. Executive Summary

The henyey catchup and history subsystem is split across two crates: `henyey-history` (archive management, catchup orchestration, verification, publishing, checkpoint building) and `henyey-historywork` (download work items, batch scheduling, publish work items). Together they implement the core catchup, history publishing, and verification workflows defined in the specification.

The history crate is at **79% function-level parity** and the historywork crate at **49% parity** per their respective `PARITY_STATUS.md` files. These numbers understate functional coverage in some areas (catchup range computation and checkpoint arithmetic are complete) and overstate it in others (the Ledger Apply Manager is entirely absent from these crates).

### Overall Adherence Rating

| Category | Rating | Notes |
|----------|--------|-------|
| **Data Types & Encoding (§3)** | **High** | HAS parsing/serialization, FutureBucket, BucketList hash, catchup config/range all present |
| **History Archive Structure (§4)** | **Full** | Path construction, checkpoint frequency, file layout, well-known endpoint all correct |
| **Checkpoint Publishing (§5)** | **Medium** | Incremental building + finalization present; missing differential upload, SCP file writing, publish queue backpressure, remote upload pipeline |
| **Catchup Range Computation (§6)** | **Full** | All 5 cases implemented with invariant checks matching spec exactly |
| **Ledger Apply Manager (§7)** | **Not present** | Entirely absent from these crates; likely in `crates/app/` or equivalent |
| **Catchup Pipeline (§8)** | **High** | 7-step catchup process in `CatchupManager`; missing herder consistency work, separate bucket HAS fetch |
| **Ledger Chain Verification (§9)** | **Medium** | Forward chain verification present; spec requires backward (highest→lowest) direction with trust establishment from SCP |
| **Bucket Application (§10)** | **High** | Disk-backed bucket loading, hash verification, live + hot archive restore; missing `differingBuckets()` (downloads all buckets) |
| **Transaction Replay (§11)** | **High** | Re-execution and metadata replay modes; per-checkpoint download; tx set/result hash verification |
| **Buffered Ledger Application (§12)** | **Not present** | Part of Ledger Apply Manager, not in these crates |
| **Error Handling & Recovery (§13)** | **Medium** | Archive failover, crash-safe checkpoint building present; missing fatal catchup failure flag; crash recovery during catchup handled via two-window design (§14.5) |
| **Invariants (§14)** | **High** | Most invariants enforced; INV-C5 (per-ledger hash check after replay) and INV-C7 (buffer monotonicity) not applicable here |
| **Constants (§15)** | **High** | All core constants present; PUBLISH_QUEUE_MAX_SIZE/UNBLOCK not enforced |

**Estimated behavioral coverage: ~70%** of the spec's requirements are implemented across these two crates. The remaining 30% falls into three categories: (1) the Ledger Apply Manager / buffered ledger application (~15%), which lives outside these crates; (2) publishing pipeline gaps (~10%); and (3) verification direction and error handling refinements (~5%).

---

## 2. Evaluation Methodology

This evaluation compares the henyey history and historywork implementations against the `CATCHUP_SPEC.md` specification (1,310 lines, 17 sections). Every section of the spec was read in full and compared against the corresponding Rust source files.

Each behavior is assessed on three dimensions:

1. **Structural completeness**: Are the required data structures, abstractions, and state machines present?
2. **Behavioral correctness**: Do the implementations follow the same algorithms, state transitions, and edge case handling?
3. **Constant fidelity**: Do hardcoded values, thresholds, and timeouts match?

Ratings per requirement:

| Symbol | Meaning |
|--------|---------|
| ✅ | Fully implemented and matches spec |
| ⚠️ | Partially implemented or minor deviation |
| ❌ | Not implemented |
| ➖ | Not applicable (pre-protocol-24 only or out-of-scope for these crates) |

Source file references use the format `file.rs:line`.

**Crate scope note**: The spec covers behaviors that span multiple crates. Sections §7 (Ledger Apply Manager) and §12 (Buffered Ledger Application) describe components that live outside these two crates. They are evaluated here for completeness but gaps are not penalized in the overall rating.

---

## 3. Section-by-Section Evaluation

### 3.1 Data Types and Encoding (Spec §3)

**Source files:** `archive_state.rs`, `catchup_range.rs`, `checkpoint.rs`, `historywork/src/lib.rs`

#### HAS Structure (§3.1)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| HAS JSON parsing with `version`, `server`, `currentLedger`, `currentBuckets` | ✅ | `archive_state.rs:1-100` — `HistoryArchiveState` struct with serde JSON support |
| `networkPassphrase` field for version ≥ 2 | ✅ | `archive_state.rs` — `network_passphrase` field present |
| `hotArchiveBuckets` for version ≥ 2 | ✅ | `archive_state.rs` — `hot_archive_buckets: Option<Vec<HASBucketLevel>>` |
| `HistoryStateBucket` with `curr`, `snap`, `next` fields | ✅ | `archive_state.rs` — `HASBucketLevel` struct |
| `FutureBucket` state encoding (state 0/1/2) | ✅ | `archive_state.rs` — `HASBucketNext` with `state`, `output`, `curr`, `snap`, `shadow` |
| Level count invariant (11 levels) | ✅ | Enforced by BucketList restoration in `catchup.rs` |
| Shadows removed for current protocols | ✅ | `archive_state.rs` — `futures_all_clear()` checks state==0; shadow field exists but unused |

#### BucketList Hash Computation (§3.2)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Per-level hash: `SHA256(currHash \|\| snapHash)` | ✅ | Computed in `henyey-bucket` crate during BucketList hash computation |
| Live hash: `SHA256(levelHash[0] \|\| ... \|\| levelHash[N-1])` | ✅ | Delegated to BucketList crate |
| Version ≥ 2: `SHA256(liveHash \|\| hotHash)` | ✅ | Combined hash computed in `replay.rs:47` for p23+ eviction |

#### Differential Bucket Sets (§3.3)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `differingBuckets(other)` — compute differential download set | ❌ | Not implemented. `CatchupManager` downloads all bucket hashes from the HAS via `all_bucket_hashes()` rather than computing a differential against local state |
| Inhibit set with zero hash and existing buckets | ❌ | No inhibit set logic |
| Sorted largest-to-smallest, snapshots before currents | ❌ | No ordering optimization |

#### Catchup Configuration (§3.4)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `toLedger` with sentinel 0 = "latest from archive" | ✅ | `catchup_range.rs` — `CatchupMode::Minimal` resolves to archive's latest |
| `count` field (UINT32_MAX = complete, 0 = minimal) | ✅ | `catchup_range.rs:35-50` — `CatchupMode` enum with `Minimal`, `Complete`, `Recent(u32)` |
| `mode` field: ONLINE / OFFLINE_BASIC / OFFLINE_COMPLETE | ⚠️ | No explicit mode enum. Online vs offline distinction is implicit in `CatchupManager` behavior. No `OFFLINE_BASIC` vs `OFFLINE_COMPLETE` distinction for validation scope |

#### Catchup Range and Ledger Range (§3.5–3.7)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `CatchupRange` with `applyBuckets`, `applyBucketsAtLedger`, `replayRange` | ✅ | `catchup_range.rs:60-80` — `CatchupRange` struct with all fields |
| `LedgerRange` with `first`, `count`, `last()`, `limit()` | ✅ | `catchup_range.rs:20-55` — `LedgerRange` struct with derived methods |
| `CheckpointRange` with `first`, `count`, `frequency` | ✅ | `historywork/src/lib.rs` — `CheckpointRange` struct |

#### File Transfer Info (§3.8)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| File types: BUCKET, LEDGER, TRANSACTIONS, RESULTS, SCP | ✅ | `historywork/src/lib.rs` — `HistoryFileType` enum with all variants |

#### Ledger Verification Status (§3.9)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Status types: OK, ERR_BAD_HASH, ERR_BAD_LEDGER_VERSION, ERR_OVERSHOT, ERR_UNDERSHOT, ERR_MISSING_ENTRIES | ⚠️ | `error.rs` has `InvalidSequence`, `InvalidPreviousHash`, `InvalidTxSetHash` but no dedicated `VerificationStatus` enum mapping to all spec statuses. Errors are reported via `HistoryError` variants |

---

### 3.2 History Archive Structure (Spec §4)

**Source files:** `paths.rs`, `archive.rs`, `archive_state.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Hierarchical hex directory structure (`XX/YY/ZZ/`) | ✅ | `paths.rs:1-50` — `hex_dir()`, `remote_dir()`, `remote_name()` functions |
| Well-known endpoint `.well-known/stellar-history.json` | ✅ | `paths.rs` — `root_has_path()` returns well-known path; `historywork/src/lib.rs` uses it |
| Path construction for bucket files (64-char hex hash) | ✅ | `paths.rs` — `bucket_path()` |
| Path construction for checkpoint files (8-char hex) | ✅ | `paths.rs` — `checkpoint_path()`, `checkpoint_file_path()` |
| HAS files use `.json` suffix | ✅ | `paths.rs` — `has_path()` |
| Checkpoint frequency = 64 | ✅ | `paths.rs` — `CHECKPOINT_FREQUENCY = 64` |
| Checkpoint boundaries at `(N × 64) - 1` | ✅ | `checkpoint.rs:1-50` — `checkpoint_containing()` |
| First checkpoint contains ledgers 1–63 (63 ledgers) | ✅ | `checkpoint.rs` — `size_of_checkpoint()` returns 63 for first checkpoint |
| `checkpointContaining(L)` formula | ✅ | `checkpoint.rs` — matches `⌊L/freq + 1⌋ × freq - 1` |
| `firstInCheckpointContaining(L)` | ✅ | `checkpoint.rs` — `first_ledger_in_checkpoint()` |
| `lastBeforeCheckpointContaining(L)` | ✅ | `checkpoint.rs` — `last_before_checkpoint()` |
| `sizeOfCheckpointContaining(L)` | ✅ | `checkpoint.rs` — `size_of_checkpoint()` |
| Ledger header file: one entry per ledger, no gaps | ✅ | Enforced during both publishing (`publish.rs`) and verification (`verify.rs`) |
| Transaction/result files may have gaps | ✅ | Handled in `catchup.rs:1594-1625` — missing entries produce empty tx sets |

---

### 3.3 Checkpoint Publishing Pipeline (Spec §5)

**Source files:** `publish.rs`, `publish_queue.rs`, `checkpoint_builder.rs`, `historywork/src/lib.rs`

#### Incremental Building (§5.2)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Lazy stream opening on first write | ✅ | `checkpoint_builder.rs:190-229` — `ensure_open()` opens writers on demand |
| Append `LedgerHeaderHistoryEntry` for every ledger | ✅ | `checkpoint_builder.rs:237-252` — `append_ledger_header()` |
| Append transactions/results only for non-empty tx sets | ✅ | `checkpoint_builder.rs:261-283` — `append_transaction_set()` called conditionally |
| Write to `.dirty` temporary files with fsync | ✅ | `checkpoint_builder.rs:78-140` — `XdrStreamWriter` writes to dirty path, `file.sync_all()` |

#### Checkpoint Finalization (§5.3)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Close all output streams at checkpoint boundary | ✅ | `checkpoint_builder.rs:290-338` — `checkpoint_complete()` finishes all writers |
| Atomic rename from dirty to canonical | ✅ | `checkpoint_builder.rs:322-333` — `fs::rename(dirty, final_path)` |
| Skip rename if canonical already exists | ⚠️ | Not explicitly checked — `fs::rename` will overwrite on most OSes |

#### HAS Queue (§5.4)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| HAS construction from BucketList | ✅ | `publish.rs:126-192` — `build_history_archive_state()` with live + hot archive |
| HAS serialized and written to queue file | ✅ | `publish_queue.rs:146-168` — `enqueue()` writes HAS JSON to SQLite |
| Persistent queue backed by database | ✅ | `publish_queue.rs:65-68` — `PublishQueue` backed by SQLite `publishqueue` table |

#### Archive Upload (§5.5)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Resolve: wait for BucketList merges | ❌ | No merge resolution waiting before upload |
| Write SCP messages to checkpoint file | ⚠️ | `PublishScpHistoryWork` exists in `historywork/src/lib.rs` but full pipeline not wired |
| Download archive's current HAS for differential | ❌ | `PublishManager::publish_checkpoint()` writes all files without differential check |
| Compute differing files via `differingBuckets` | ❌ | Not implemented; all buckets are written |
| Gzip files for upload | ✅ | `publish.rs:376-399` — `write_xdr_gz()` |
| Upload HAS to permanent + well-known locations | ⚠️ | `PublishHistoryArchiveStateWork` in historywork writes to both paths, but full remote upload pipeline is incomplete |
| Cleanup: delete local copies after upload | ❌ | Not implemented |

#### Publish Queue Backpressure (§5.6)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `PUBLISH_QUEUE_MAX_SIZE = 16` gating | ❌ | No constant or enforcement found in either crate |
| `PUBLISH_QUEUE_UNBLOCK_APPLICATION = 8` resume threshold | ❌ | Not implemented |
| Backpressure pauses transaction replay during offline catchup | ❌ | Not implemented |

#### Crash Recovery (§5.7)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Dirty file cleanup on startup | ✅ | `checkpoint_builder.rs:340-436` — `cleanup()` with `scan_for_dirty_files()` |
| Both dirty+final exist → delete dirty | ✅ | `checkpoint_builder.rs:403-410` |
| Only dirty exists → truncate to LCL | ⚠️ | `checkpoint_builder.rs:412-421` — deletes instead of truncating; comment says "simplified" |
| Only final exists → validate ends at correct ledger | ⚠️ | `checkpoint_builder.rs:422-429` — logs debug but does not validate content |
| Stale HAS queue files above LCL removed | ❌ | Not implemented in `PublishQueue` |

---

### 3.4 Catchup Configuration and Range Computation (Spec §6)

**Source files:** `catchup_range.rs`

#### Configuration Resolution (§6.1)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `toLedger = 0` resolved to latest archive checkpoint | ✅ | `CatchupManager::catchup_to_ledger()` in `catchup.rs` downloads root HAS and uses `current_ledger` |

#### Range Computation (§6.2)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Case 1: `lcl > GENESIS` → replay only `[lcl+1, toLedger]` | ✅ | `catchup_range.rs` — `ExistingBucketState` path for replay-only |
| Case 2: `lcl == GENESIS` AND `count ≥ fullReplayCount` → full replay | ✅ | `catchup_range.rs` — `Complete` mode with full range |
| Case 3: `lcl == GENESIS`, `count == 0`, target at checkpoint → buckets only | ✅ | `catchup_range.rs` — `Minimal` mode, checkpoint boundary check |
| Case 4: `lcl == GENESIS`, `firstInCheckpoint(...) == GENESIS` → full replay | ✅ | `catchup_range.rs` — handled as sub-case of `Recent` mode |
| Case 5: otherwise → buckets at `lastBeforeCheckpoint(...)` + replay | ✅ | `catchup_range.rs` — standard `Recent` mode with bucket apply + replay |

#### Range Invariants (§6.3)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| At least one operation required | ✅ | `catchup_range.rs` — invariant check after computation |
| Both ops: `applyBucketsAtLedger + 1 == replayRange.first` | ✅ | `catchup_range.rs` — explicit assertion |
| Bucket only: replayRange empty | ✅ | `catchup_range.rs` — verified |
| Replay only: applyBucketsAtLedger == 0 | ✅ | `catchup_range.rs` — verified |

**This section is at full parity with the spec.**

---

### 3.5 Ledger Apply Manager (Spec §7)

**Note**: The Ledger Apply Manager is **not implemented** in `crates/history/` or `crates/historywork/`. This component sits between the herder and the ledger close pipeline, and likely resides in `crates/app/` or the main binary crate.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Ledger buffering in map keyed by sequence | ❌ | Not in these crates |
| Buffer spans at most two checkpoints (~128 ledgers) | ❌ | Not in these crates |
| `processLedger` decision tree | ❌ | Not in these crates |
| Sequential application via `tryApplySyncingLedgers` | ❌ | Not in these crates |
| `MAX_EXTERNALIZE_LEDGER_APPLY_DRIFT = 12` check | ❌ | Not in these crates |
| Online catchup trigger conditions | ❌ | Not in these crates |
| Buffer trimming to checkpoint boundaries | ❌ | Not in these crates |

**Evaluation note**: These gaps are not penalized in the overall rating for these crates since the functionality belongs to a different module. However, it is important to note that this critical coordination layer has no equivalent in these crates.

---

### 3.6 Catchup Pipeline (Spec §8)

**Source files:** `catchup.rs`, `lib.rs`, `historywork/src/lib.rs`

#### Phase 1: Fetch HAS (§8.2)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Download HAS from randomly selected archive | ✅ | `catchup.rs` — `CatchupManager` tries archives in configured order with failover |
| Validate network passphrase | ⚠️ | HAS parsing includes `network_passphrase` field but no explicit passphrase validation check found in catchup path |
| Validate HAS checkpoint > LCL | ✅ | Implicit in catchup range computation |
| Separate HAS for bucket checkpoint if different from target | ⚠️ | `CatchupManager` fetches a single HAS; separate bucket-checkpoint HAS is not explicitly supported |

#### Phase 2: Download and Verify Ledger Chain (§8.3)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Download ledger headers for entire range (batch, one per checkpoint) | ✅ | `catchup.rs:1534-1647` — `download_ledger_data()` downloads per-checkpoint |
| Verify ledger hash chain | ✅ | `catchup.rs:1692-1738` — `verify_downloaded_data()` calls `verify::verify_header_chain()` |
| Sequential: download first, then verify | ✅ | Data downloaded then verified in `verify_downloaded_data()` |

#### Phase 3: Build Catchup Sequence (§8.4)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Herder consistency work (set tracking state) | ❌ | Not implemented in these crates |
| Tx result verification (offline-complete mode) | ⚠️ | Tx result hash verification exists in `verify.rs:135+` but no offline-complete mode gating |
| Bucket download → verify → apply sequence | ✅ | `CatchupManager` downloads buckets, verifies hashes, applies via `apply_buckets()` |
| Transaction download → apply sequence | ✅ | `CatchupManager::replay_via_close_ledger()` |

#### Phase 4: Apply Buffered Ledgers (§8.5)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Set LCL from verified chain after bucket apply | ✅ | Done in `catchup.rs` after `apply_buckets()` |
| Drain buffered ledger queue | ❌ | Not in these crates (Ledger Apply Manager) |

#### Completion (§8.6)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Verify last applied ledger hash matches expected | ✅ | Verified during replay loop |
| Clear catchup work reference | ✅ | `CatchupManager` is consumed after completion |

---

### 3.7 Ledger Chain Verification (Spec §9)

**Source files:** `verify.rs`, `historywork/src/lib.rs`

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Verification direction: backwards (highest → lowest) | ❌ | `verify.rs:56-86` — verifies **forward** (oldest-first within a flat header slice). Spec requires inter-checkpoint verification from highest checkpoint downward with hash-link passing |
| Trust establishment from SCP consensus hash | ❌ | No trusted hash parameter; verification is purely structural (sequential check) |
| Trust from explicit hash for offline catchup | ❌ | Not supported |
| Per-entry: verify `SHA256(header) == storedHash` | ✅ | `verify.rs:74` — `compute_header_hash()` |
| Per-entry: verify sequential sequence numbers | ✅ | `verify.rs:66-71` — checks `curr.seq == prev.seq + 1` |
| Per-entry: verify `prev.hash == curr.previousLedgerHash` | ✅ | `verify.rs:76-82` — hash chain check |
| LCL comparison (`entry.seq == LCL.seq` → hash match) | ❌ | Not implemented; no LCL parameter in `verify_header_chain()` |
| Unsupported ledger version detection | ❌ | No `ledgerVersion > supportedMaxVersion` check |
| Outgoing/incoming hash link between checkpoints | ❌ | No inter-checkpoint hash-link mechanism |
| Fatal failure flag when chain disagrees with local state | ❌ | No fatal failure flag |
| `DownloadLedgerHeadersWork` with chain verification | ✅ | `historywork/src/lib.rs` — `DownloadLedgerHeadersWork` with chain verification callback |

**Analysis**: The core hash-chain verification (sequential headers, hash linking) is correct within a single checkpoint file. However, the spec's inter-checkpoint backward verification with trust establishment from SCP is architecturally different from henyey's approach, which verifies a flat list of headers forward. For correctness, the forward approach produces the same result when all headers are available and the first header can be trusted. The gap is in trust establishment — henyey does not anchor verification to an SCP-provided hash.

---

### 3.8 Bucket Application (Spec §10)

**Source files:** `catchup.rs`, `historywork/src/lib.rs`

#### Bucket Download (§10.1)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Differential download via `differingBuckets(localHAS)` | ❌ | Downloads all bucket hashes from HAS via `all_bucket_hashes()` — no differential |
| File size check ≤ `MAX_HISTORY_ARCHIVE_BUCKET_SIZE` (100 GB) | ✅ | `archive_state.rs` — `MAX_HISTORY_ARCHIVE_BUCKET_SIZE` constant defined |
| SHA-256 hash verification during download | ✅ | `historywork/src/lib.rs` — `DownloadBucketsWork` verifies hash; `verify.rs:98-109` — `verify_bucket_hash()` |
| Parallel bucket downloads | ✅ | `historywork/src/lib.rs` — `MAX_CONCURRENT_DOWNLOADS = 16` with parallel download |
| Adopt verified bucket into bucket manager | ✅ | `catchup.rs` — buckets loaded and passed to `BucketList::restore_from_has()` |

#### Bucket Application Algorithm (§10.2)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Index buckets (parallel) | ✅ | Bucket indexing done during disk-backed loading in `catchup.rs` |
| Apply entries in priority order (level 0 curr first) | ✅ | Handled by BucketList restoration in `henyey-bucket` crate |
| Skip keys seen in higher-priority buckets | ✅ | Delegated to `henyey-bucket` BucketApplicator |
| Assume HAS state (adopt buckets, restart merges) | ✅ | `catchup.rs:1307-1312` — `BucketList::restore_from_has()` with next states |

#### Post-Bucket-Apply (§10.3)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Set LCL to verified ledger at bucket checkpoint | ✅ | Done in catchup orchestration |
| Store ledger header in persistent storage | ✅ | `catchup.rs:1740-1800` — `persist_ledger_history()` |
| Hot archive bucket list restore (p24+) | ✅ | `catchup.rs:1339-1520` — full hot archive BucketList restoration |
| Contract module compilation (p25+) | ⚠️ | Soroban module cache exists (`henyey-tx::PersistentModuleCache`) but post-bucket-apply compilation step not verified in catchup path |

---

### 3.9 Transaction Replay (Spec §11)

**Source files:** `replay.rs`, `catchup.rs`

#### Per-Checkpoint Workflow (§11.2)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Download and decompress transaction file | ✅ | `catchup.rs:1649-1690` — downloads per checkpoint from archives |
| Read `LedgerHeaderHistoryEntry` from header file | ✅ | `catchup.rs:1566-1577` — finds header by sequence |
| Read transaction set from transaction file | ✅ | `catchup.rs:1579-1625` — extracts tx set with protocol-aware empty set creation |
| Verify tx set hash matches header's `scpValue.txSetHash` | ✅ | `catchup.rs:1703-1719` — `verify::verify_tx_set()` |
| Verify `previousLedgerHash` matches LCL hash | ✅ | `catchup/replay.rs::knit_to_lcl_decision` (case 4) — explicit pre-apply check, fatal on mismatch |
| Knit-to-LCL 5-case decision matrix | ✅ | `catchup/replay.rs::knit_to_lcl_decision` — cases 1/2/3/4/5 mirror `ApplyCheckpointWork::getNextLedgerCloseData()` (stellar-core/src/catchup/ApplyCheckpointWork.cpp:205-262); case 5 (overshoot) and case 2/3 hash checks raise dedicated fatal variants (`KnitOvershot`, `KnitLclPredecessorHashMismatch`, `KnitLclHashMismatch`, `KnitCurrentLedgerPrevHashMismatch`) |
| Apply via normal ledger close pipeline | ✅ | `replay.rs` — `replay_ledger_with_execution()` uses `execute_transaction_set()` |
| Verify resulting ledger hash matches expected | ✅ | `replay.rs` — post-execution hash verification |
| Cleanup: delete temporary files | ❌ | No explicit cleanup of downloaded checkpoint files |

#### Ordering and Dependencies (§11.3)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Checkpoints processed sequentially | ✅ | `download_ledger_data()` processes checkpoints in order |
| Downloads may overlap with application | ❌ | Downloads and application are sequential, not pipelined |
| Wait for pending BucketList merges before each ledger | ⚠️ | BucketList merges are resolved synchronously but no explicit conditional wait |

#### Backpressure (§11.4)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Publish queue gating (`PUBLISH_QUEUE_MAX_SIZE`) | ❌ | Not implemented |
| Resume at `PUBLISH_QUEUE_UNBLOCK_APPLICATION` | ❌ | Not implemented |

#### Transaction File Gaps (§11.5)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Handle missing tx entries for empty ledgers | ✅ | `catchup.rs:1594-1625` — creates empty tx set when no entry found |
| Protocol-aware empty set (generalized for p20+) | ✅ | `catchup.rs:1597-1625` — creates proper `GeneralizedTransactionSet` with empty phases |

---

### 3.10 Buffered Ledger Application (Spec §12)

**Note**: Like the Ledger Apply Manager (§7), this is **not implemented** in these crates.

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Query buffer for next ledger (LCL + 1) | ❌ | Not in these crates |
| Wait for BucketList merges before apply | ❌ | Not in these crates |
| Apply via normal ledger close pipeline | ❌ | Not in these crates |
| Transition to normal operation after drain | ❌ | Not in these crates |

---

### 3.11 Error Handling and Recovery (Spec §13)

**Source files:** `error.rs`, `catchup.rs`, `checkpoint_builder.rs`, `archive.rs`

#### Retry Semantics (§13.1)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Catchup pipeline: no retry (single attempt) | ✅ | `CatchupManager` runs once, returns `Result` |
| HAS download: up to 10 retries with archive rotation | ⚠️ | `download.rs:37` — `DEFAULT_RETRIES = 3`, not 10; archive failover exists |
| Ledger header download: retries with archive rotation | ✅ | `catchup.rs:1649-1674` — tries each archive in sequence |
| Bucket download: retries with archive rotation | ✅ | `historywork/src/lib.rs` — `DownloadBucketsWork` with retry per bucket |
| Transaction file download: retries with archive rotation | ✅ | `catchup.rs:1649-1674` — archive failover |
| Verification, bucket apply, replay: no retry | ✅ | All return errors immediately |

#### Archive Rotation (§13.2)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Rotate to different archive on failure | ✅ | `CatchupManager` iterates through archives in `catchup.rs:1654-1673` |
| Archive classification (read-only, read-write) | ⚠️ | `ArchiveConfig` in `lib.rs` supports read/write flags but no preference for read-only during downloads |

#### Fatal Catchup Failure (§13.3)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| `catchupFatalFailure` flag | ❌ | Not implemented |
| Prevent further catchup attempts | ❌ | Not implemented |
| Require manual intervention | ❌ | Not implemented |

#### Crash Recovery During Catchup (§13.4)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Checkpoint builder recovers on restart | ✅ | `checkpoint_builder.rs:340-436` — `cleanup()` method |
| LCL restored from persistent storage | ✅ | Handled by database layer |
| Crash after bucket apply → re-catch up from advanced LCL | ⚠️ | Two-window design: (1) startup restore path is safe (reads `last_closed_ledger`/HAS, not ahead-of-LCL rows); (2) `CATCHUP_PERSIST_PENDING` sentinel forces full bucket-apply on restart if deferred persist was interrupted. **Gap:** CLI readers (`publish_history`, `self_check`) still use `MAX(ledgerseq)` and may observe ahead-of-LCL state after a Window-1 crash. See `crates/app/README.md` §14.5 design note. |
| Crash before bucket apply → catch up from scratch | ✅ | No partial state to recover |

#### Hash Verification Failures (§13.5)

| Requirement | Status | Evidence |
|-------------|--------|----------|
| Bucket: retry on hash failure | ✅ | Archive failover on download failure |
| Headers: report specific error type | ✅ | `error.rs` — `InvalidSequence`, `InvalidPreviousHash` variants |
| Transaction sets: fatal error on hash mismatch | ✅ | `verify.rs` — `verify_tx_set()` returns error |

---

### 3.12 Invariants and Safety Properties (Spec §14)

| Invariant | Status | Evidence |
|-----------|--------|----------|
| **INV-C1**: Hash chain integrity (previousLedgerHash) | ✅ | `verify.rs:56-86` — verified in header chain; also during replay |
| **INV-C2**: Bucket SHA-256 hash verification | ✅ | `verify.rs:98-109` — `verify_bucket_hash()` |
| **INV-C3**: Transaction set hash matches header | ✅ | `verify.rs` — `verify_tx_set()` |
| **INV-C4**: BucketList hash after bucket apply | ✅ | Verified via `verify_ledger_hash()` in `verify.rs:124+` |
| **INV-C5**: Ledger hash after replay matches header | ✅ | `replay.rs` — post-execution verification |
| **INV-C6**: Catchup range consistency | ✅ | `catchup_range.rs` — explicit assertions on range relationships |
| **INV-C7**: Buffer monotonicity (`lastQueuedToApply`) | ➖ | Not in these crates (Ledger Apply Manager) |
| **INV-C8**: Checkpoint completeness | ✅ | Enforced during publishing and verification |
| **INV-C9**: Merge resolution before apply | ⚠️ | BucketList merges resolved synchronously but no explicit conditional wait check |
| **INV-C10**: Archive file immutability | ✅ | Content-hash naming for buckets; checkpoint files written once |

---

### 3.13 Constants (Spec §15)

| Constant | Spec Value | Henyey Value | Status |
|----------|------------|--------------|--------|
| `CHECKPOINT_FREQUENCY` | 64 | 64 (`paths.rs`) | ✅ |
| `GENESIS_LEDGER_SEQ` | 1 | 1 (used in `catchup_range.rs`) | ✅ |
| `PUBLISH_QUEUE_MAX_SIZE` | 16 | Not defined | ❌ |
| `PUBLISH_QUEUE_UNBLOCK_APPLICATION` | 8 | Not defined | ❌ |
| `MAX_EXTERNALIZE_LEDGER_APPLY_DRIFT` | 12 | Not in these crates | ➖ |
| `MAX_HISTORY_ARCHIVE_BUCKET_SIZE` | 100 GB | 100 GB (`archive_state.rs`) | ✅ |
| `HAS_VERSION_BEFORE_HOT_ARCHIVE` | 1 | Implicit (version field) | ✅ |
| `HAS_VERSION_WITH_HOT_ARCHIVE` | 2 | 2 (`publish.rs:184`) | ✅ |
| `MAX_CONCURRENT_DOWNLOADS` | (impl detail) | 16 (`historywork/src/lib.rs`) | ✅ |

---

## 4. Gap Summary

### Critical Gaps

| Gap | Spec Section | Impact |
|-----|-------------|--------|
| **Ledger chain verification direction** | §9.2–9.3 | Henyey verifies forward; spec requires backward with trust anchoring from SCP hash. The forward approach is functionally correct when all headers are available but lacks the trust establishment model. Could accept a corrupt header file if the first header is not independently verified. |
| **No `differingBuckets()` differential download** | §3.3, §10.1 | All buckets are downloaded regardless of local state. For nodes with existing state performing catchup, this wastes bandwidth and time. |
| **Fatal catchup failure flag** | §13.3 | Without this flag, a node with corrupted local state could repeatedly attempt and fail catchup instead of halting. |

### Moderate Gaps

| Gap | Spec Section | Impact |
|-----|-------------|--------|
| **Publish queue backpressure** | §5.6, §11.4 | During offline catchup, replay can outpace publishing, causing unbounded queue growth |
| **Differential archive upload** | §5.5 | All files are uploaded instead of only new/changed files, increasing publish time and bandwidth |
| **SCP message publishing** | §5.5 | Work items exist in historywork but not wired into the full publish pipeline |
| **Crash recovery truncation** | §5.7 | Partial dirty files are deleted instead of truncated to LCL; checkpoint must be fully rebuilt |
| **HAS download retry count** | §13.1 | 3 retries vs spec's 10 retries |
| **Online/offline mode distinction** | §3.4 | No `ONLINE`/`OFFLINE_BASIC`/`OFFLINE_COMPLETE` mode enum; validation scope is not configurable |
| **Network passphrase validation during catchup** | §8.2 | HAS field exists but no explicit validation against configured passphrase |

### Minor Gaps

| Gap | Spec Section | Impact |
|-----|-------------|--------|
| **Pipelined download+apply** | §11.3 | Downloads and application are sequential; spec allows overlapping for performance |
| **Checkpoint finalization skip-if-exists** | §5.3 | Rename may overwrite instead of skipping |
| **Verification status enum** | §3.9 | Errors reported via `HistoryError` variants instead of a dedicated status enum |
| **Stale HAS queue cleanup** | §5.7 | Queue files above LCL not cleaned on startup |
| **Temporary file cleanup after replay** | §11.2 | Downloaded checkpoint files not explicitly deleted |

---

## 5. Risk Assessment

### Consensus Safety

The critical path for consensus safety is **bucket application + transaction replay + hash verification**. These are all implemented correctly:

- Bucket hash verification (INV-C2) ✅
- Transaction set hash verification (INV-C3) ✅
- BucketList hash after bucket apply (INV-C4) ✅
- Ledger hash after replay (INV-C5) ✅
- Catchup range consistency (INV-C6) ✅

The **verification direction** gap (forward vs backward) does not affect consensus safety when all headers are available, which is always the case during catchup. The risk is theoretical: a corrupted archive could provide internally consistent but incorrect headers. In practice, this is mitigated by the post-replay hash verification (INV-C5) which catches any divergence.

The **missing fatal catchup failure flag** is a safety risk for nodes with corrupted local state. Such nodes would repeatedly attempt catchup rather than halting, potentially consuming resources without recovering.

### Operational Risk

The **missing `differingBuckets()`** is a significant efficiency concern. For a node re-catching up (e.g., after a partial failure), downloading all ~22 buckets (potentially many GB) instead of only the differential set adds substantial time and bandwidth.

The **missing publish queue backpressure** could cause problems during long offline catchup replays where publishing falls behind.

### Overall Risk: **Medium**

Catchup correctness is solid. The gaps are primarily in efficiency (differential downloads, pipelined replay), operational robustness (fatal failure flag, publish backpressure), and trust establishment (verification direction).

---

## 6. Recommendations

### Priority 1 (Critical — affects correctness guarantees)

1. **Implement backward ledger chain verification with trust anchoring** (§9.2–9.3): Refactor `verify_header_chain()` to accept a trusted hash from SCP (or explicit configuration) and verify from highest checkpoint downward with inter-checkpoint hash links.

2. **Implement fatal catchup failure flag** (§13.3): When verification detects chain disagreement with local state and the trust hash came from SCP, set a persistent flag preventing further catchup attempts.

### Priority 2 (Moderate — affects efficiency and robustness)

3. **Implement `differingBuckets()` for differential download** (§3.3, §10.1): Compare remote HAS against local HAS to download only new/changed buckets. This is important for operational efficiency, especially for nodes re-catching up.

4. **Implement publish queue backpressure** (§5.6): Add `PUBLISH_QUEUE_MAX_SIZE = 16` and `PUBLISH_QUEUE_UNBLOCK_APPLICATION = 8` constants and gating logic in the replay loop.

5. **Add online/offline mode distinction** (§3.4): Introduce a mode enum and use it to control validation scope (e.g., tx result verification only in `OFFLINE_COMPLETE` mode).

6. **Increase HAS download retry count** to 10 (§13.1).

### Priority 3 (Minor — improves completeness)

7. **Implement differential archive upload** in the publish pipeline (§5.5).

8. **Improve crash recovery**: Truncate partial dirty files to LCL instead of deleting them (§5.7).

9. **Pipeline download+apply** for transaction replay across checkpoints (§11.3).

10. **Add network passphrase validation** during catchup HAS fetch (§8.2).

---

## 7. v26.0.1 Implementation Delta

This section documents changes introduced in stellar-core between v25.0.1 and v26.0.1 that affect the catchup and history subsystems, and assesses henyey's alignment.

### Key stellar-core v26 Changes

| Commit | Change | Impact on Catchup/History |
|--------|--------|---------------------------|
| `e58bf7d` | **Catch runtime errors in catchup** — wraps `getNextLedgerCloseData()` in `ApplyCheckpointWork` and `DownloadApplyTxsWork` with try/catch for `std::runtime_error`, returning `WORK_FAILURE` instead of crashing | Error resilience during ledger apply within catchup pipeline |
| `5de9a43` | **History test fixes** — removes dead `HistoryManager` methods, simplifies test utilities | Cleanup only; no behavioral change |
| `e9c07eb` | **Clear prepared statements cache at end of catchup** | Post-catchup DB cleanup |
| `87e2161` | **CAP-77: Freeze ledger keys via network configuration** | Protocol-level change; indirectly affects bucket contents |
| `dc44d5b` | **Remove maintenance** — removes periodic maintenance scheduling from history | Simplifies operational model |
| `1a8409f` | **Refactor snapshots to single BucketListSnapshot class** | Internal refactor; changes snapshot access patterns |
| `c8a3f1f` | **Drop prepared statement cache** | DB layer cleanup |
| `87f71c6` | **SQLite parallel apply support** — database splitting for background apply | Performance improvement for catchup apply |
| `dcfbcd2` | **Minor improvements to publishing** | Publishing quality-of-life |
| `5a618d0` | **Fix missing durable writes in CheckpointBuilder** | Durability fix directly relevant to crash recovery |
| `cde79db` | **Bucket race fixes** | Correctness fix for concurrent bucket operations |
| `f757496` | **Fix race in index creation on merge** | Bucket index correctness |

### Henyey Alignment Assessment

#### Runtime Error Handling in Catchup (e58bf7d)

stellar-core v26 added explicit `try/catch` blocks around `getNextLedgerCloseData()` in `ApplyCheckpointWork::onRun()` and similar error wrapping in `DownloadApplyTxsWork`. This prevents unhandled runtime errors from crashing the node during catchup.

**Henyey status: ✅ Aligned by design.** Henyey's Rust-based catchup uses `Result<T, E>` return types throughout the replay path. Runtime errors (panics) in Rust are not expected in normal operation and would indicate a bug. The `replay_ledger_with_execution()` function and `CatchupManager` orchestration propagate errors via `?` operator, achieving the same graceful failure without the need for explicit catch blocks. Panics in test assertions (`persist.rs`) are expected test-only behavior.

#### History Test Fixes (5de9a43)

stellar-core removed dead methods from `HistoryManager` (`CATCHUP_RECENT`, `CATCHUP_COMPLETE`, `CATCHUP_MINIMAL` constants moved to test-only) and simplified test utilities.

**Henyey status: ➖ Not applicable.** Henyey's `CatchupMode` enum (`Minimal`, `Complete`, `Recent`) is already cleanly separated from test utilities. No dead code equivalent exists.

#### Prepared Statement Cache Clearing (e9c07eb, c8a3f1f)

stellar-core added explicit clearing of the prepared statement cache at the end of catchup to free resources, and later dropped the cache entirely.

**Henyey status: ➖ Not applicable.** Henyey uses SQLite via `rusqlite` which manages statement lifetime through Rust's ownership system. No prepared statement cache requires manual clearing.

#### Missing Durable Writes in CheckpointBuilder (5a618d0)

stellar-core fixed cases where checkpoint data could be lost on crash due to missing `fsync` calls.

**Henyey status: ✅ Already addressed.** `checkpoint_builder.rs` calls `file.sync_all()` (equivalent to `fsync`) before rename operations, ensuring durability.

#### Bucket Race Fixes (cde79db, f757496)

stellar-core fixed race conditions in bucket operations during concurrent access and index creation during merges.

**Henyey status: ⚠️ Different architecture.** Henyey's bucket operations are primarily single-threaded within the catchup path. The `henyey-bucket` crate uses Rust's ownership model to prevent data races at compile time. However, the specific scenarios (concurrent bucket adoption during live operation) should be verified if/when henyey implements background bucket merges.

#### SQLite Parallel Apply (87f71c6)

stellar-core added support for splitting the database to enable parallel ledger application during catchup.

**Henyey status: ❌ Not implemented.** Henyey's catchup applies ledgers sequentially. Parallel apply is a performance optimization that could be considered for future work but does not affect correctness.

#### Remove Maintenance (dc44d5b)

stellar-core removed the periodic maintenance scheduling from the history subsystem.

**Henyey status: ✅ Aligned.** Henyey never implemented a maintenance scheduling system; maintenance operations are handled on-demand or at checkpoint boundaries.

### Summary of v26 Delta

| Area | stellar-core v26 Change | Henyey Status |
|------|------------------------|---------------|
| Runtime error handling in catchup | Added try/catch | ✅ Covered by Result types |
| History test cleanup | Removed dead code | ➖ N/A |
| Prepared statement lifecycle | Cache cleared/dropped | ➖ N/A (Rust ownership) |
| CheckpointBuilder durability | Fixed missing fsync | ✅ Already correct |
| Bucket race conditions | Fixed concurrent access | ⚠️ Architecture differs |
| Parallel apply support | Database splitting | ❌ Not implemented |
| Maintenance removal | Removed scheduler | ✅ Never implemented |

**Overall v26 impact on henyey: Low.** The most significant behavioral change (runtime error handling) is already covered by Rust's type system. The parallel apply optimization is a future performance opportunity but not a correctness requirement.
