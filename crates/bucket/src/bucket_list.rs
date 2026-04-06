//! BucketList implementation - the full hierarchical bucket structure.
//!
//! The BucketList is Stellar's core data structure for storing ledger state.
//! It consists of 11 levels (0-10), where each level contains two buckets:
//!
//! - `curr`: The current bucket being filled with new entries
//! - `snap`: The snapshot bucket from the previous spill
//!
//! # Architecture
//!
//! The bucket list is a log-structured merge tree (LSM tree) optimized for
//! Stellar's append-heavy workload. Lower levels update more frequently and
//! contain recent data, while higher levels contain older, more stable data.
//!
//! ```text
//! Level 0:  [curr] [snap]   <- Updates every 2 ledgers
//! Level 1:  [curr] [snap]   <- Updates every 8 ledgers
//! Level 2:  [curr] [snap]   <- Updates every 32 ledgers
//! ...
//! Level 10: [curr] [snap]   <- Never spills (top level)
//! ```
//!
//! # Spill Mechanics
//!
//! When a level "spills", its `curr` bucket becomes its new `snap`, and
//! the old `snap` is merged into the next level's `curr`. Spill boundaries
//! follow stellar-core's `levelShouldSpill` rules:
//!
//! - `level_size(N)` = 4^(N+1): Size boundary for level N
//! - `level_half(N)` = level_size(N) / 2: Half-size boundary
//! - A level spills when the ledger is at a half or full size boundary
//!
//! # Hash Computation
//!
//! The bucket list hash is computed by hashing all level hashes together.
//! Each level hash is `SHA256(curr_hash || snap_hash)`. This Merkle tree
//! structure enables efficient integrity verification.
//!
//! # Entry Lookup
//!
//! Lookups search from level 0 to level 10, checking `curr` then `snap`
//! at each level. The first match is returned (newer entries shadow older).
//! Dead entries (tombstones) shadow live entries, returning None.

use henyey_common::protocol::MIN_SOROBAN_PROTOCOL_VERSION;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use stellar_xdr::curr::{
    BucketListType, BucketMetadata, BucketMetadataExt, LedgerEntry, LedgerKey, Limits,
    StateArchivalSettings, WriteXdr,
};
use tokio::sync::oneshot;

use henyey_common::{BucketListDbConfig, Hash256};

use crate::bucket::Bucket;
use crate::cache::CacheStats;
use crate::entry::{
    get_ttl_key, is_persistent_entry, is_soroban_entry, is_temporary_entry, is_ttl_expired,
    BucketEntry, BucketEntryExt,
};
use crate::eviction::{
    update_starting_eviction_iterator, EvictionCandidate, EvictionIterator, EvictionIteratorExt,
    EvictionResult,
};
use crate::future_bucket::MergeKey;
use crate::index::BucketEntryCounters;
use crate::live_iterator::LiveEntriesIterator;
use crate::manager::{canonical_bucket_filename, promote_temp_to_canonical, temp_merge_path};
use crate::merge::{
    merge_buckets_to_file, merge_buckets_to_file_with_counters,
    merge_buckets_with_options_and_shadows, merge_buckets_with_options_and_shadows_and_counters,
    merge_in_memory,
};
use crate::merge_map::BucketMergeMap;
use crate::metrics::MergeCounters;
use crate::{
    protocol_version_is_before, protocol_version_starts_from, BucketError, ProtocolVersion, Result,
};

/// Number of levels in the BucketList (matches stellar-core's `kNumLevels`).
pub const BUCKET_LIST_LEVELS: usize = 11;

/// FutureBucket state constants (matches stellar-core's FBStatus enum in HAS JSON).
/// HAS_NEXT_STATE_CLEAR: No pending merge
/// HAS_NEXT_STATE_OUTPUT: Merge complete, output hash is known
/// HAS_NEXT_STATE_INPUTS: Merge in progress, input hashes are stored
pub const HAS_NEXT_STATE_CLEAR: u32 = 0;
pub const HAS_NEXT_STATE_OUTPUT: u32 = 1;
pub const HAS_NEXT_STATE_INPUTS: u32 = 2;

// ============================================================================
// Bucket list arithmetic helpers (shared by BucketList and HotArchiveBucketList)
// ============================================================================

/// Round down `value` to the nearest multiple of `modulus` (must be power-of-2).
pub(crate) fn bl_round_down(value: u32, modulus: u32) -> u32 {
    if modulus == 0 {
        return 0;
    }
    value & !(modulus - 1)
}

/// Half the idealized size of a level (matches stellar-core's levelHalf).
/// Level 0: 2, Level 1: 8, Level 2: 32, Level 3: 128, etc.
pub(crate) fn bl_level_half(level: usize) -> u32 {
    1u32 << (2 * level + 1)
}

/// Idealized size of a level for spill boundaries (matches stellar-core's levelSize).
/// Level 0: 4, Level 1: 16, Level 2: 64, Level 3: 256, etc.
pub(crate) fn bl_level_size(level: usize) -> u32 {
    1u32 << (2 * (level + 1))
}

/// Returns true if a level should spill at a given ledger.
///
/// This matches stellar-core's `levelShouldSpill`:
///   return (ledger == roundDown(ledger, levelHalf(level)) ||
///           ledger == roundDown(ledger, levelSize(level)));
///
/// Which simplifies to: ledger is a multiple of levelHalf(level).
/// For level 0 (half=2): spills at ledgers 0, 2, 4, 6, ...
/// For level 1 (half=8): spills at ledgers 0, 8, 16, 24, ...
/// For level 2 (half=32): spills at ledgers 0, 32, 64, 96, ...
pub(crate) fn bl_level_should_spill(ledger_seq: u32, level: usize, num_levels: usize) -> bool {
    if level == num_levels - 1 {
        // There's no level above the highest level, so it can't spill.
        return false;
    }

    let half = bl_level_half(level);
    let size = bl_level_size(level);
    ledger_seq % half == 0 || ledger_seq % size == 0
}

/// Returns true if tombstone (dead) entries should be kept at the given level.
/// Tombstones are kept at all levels except the deepest, where they can be dropped
/// because there is no deeper level for them to shadow.
pub(crate) fn bl_keep_tombstone_entries(level: usize, num_levels: usize) -> bool {
    level < num_levels - 1
}

/// Determines whether to merge with an empty curr bucket instead of the actual curr.
///
/// This is a critical piece of the bucket list merge algorithm that prevents data
/// duplication. When a level is about to snap its curr bucket (because the next
/// spill boundary will affect this level), we should NOT merge with curr. Instead,
/// we merge with an empty bucket and let curr be preserved until it becomes snap.
///
/// Matches stellar-core's `shouldMergeWithEmptyCurr` in BucketListBase.cpp.
pub(crate) fn bl_should_merge_with_empty_curr(
    ledger_seq: u32,
    level: usize,
    num_levels: usize,
) -> bool {
    if level == 0 {
        // Level 0 always merges with its curr
        return false;
    }

    // Round down to when the merge was started
    let merge_start_ledger = bl_round_down(ledger_seq, bl_level_half(level - 1));

    // Calculate when the next spill would happen
    let next_change_ledger = merge_start_ledger + bl_level_half(level - 1);

    // If the next spill would affect this level, use empty curr
    // because curr is about to be snapped
    bl_level_should_spill(next_change_ledger, level, num_levels)
}

/// State of a pending bucket merge from History Archive State (HAS).
///
/// When restoring from a HAS, each level may have a pending merge:
/// - State 0 (CLEAR): No pending merge
/// - State 1 (OUTPUT): Merge complete, output hash is set
/// - State 2 (INPUTS): Merge in progress, input curr/snap hashes are set
///
/// For state 1, use the output hash directly as the level's `next` bucket.
/// For state 2, restart the merge using the stored input hashes.
#[derive(Clone, Debug, Default)]
pub struct HasNextState {
    /// Merge state (0 = clear, 1 = output, 2 = inputs)
    pub state: u32,
    /// Output bucket hash if merge is complete (state == 1)
    pub output: Option<Hash256>,
    /// Input curr bucket hash for pending merge (state == 2)
    pub input_curr: Option<Hash256>,
    /// Input snap bucket hash for pending merge (state == 2)
    pub input_snap: Option<Hash256>,
}

/// Pending merge result for a bucket level.
///
/// This supports two modes matching stellar-core:
/// - `InMemory`: Synchronous merge result (used for level 0)
/// - `Async`: Background merge in progress (used for levels 1+)
///
/// The async mode allows merges to run in background threads while the
/// main thread continues processing, significantly reducing ledger close time.
pub enum PendingMerge {
    /// Synchronous merge result (level 0 only).
    /// Level 0 uses in-memory merges that complete immediately.
    InMemory(Bucket),
    /// Asynchronous merge in progress (levels 1+).
    /// The merge runs in a background thread and the result is retrieved when commit() is called.
    Async(AsyncMergeHandle),
}

/// Describes the serializable state of a pending merge for HAS persistence.
///
/// Matches the three states of stellar-core FutureBucket:
/// - State 0 (clear): no pending merge (represented by `None` at call site)
/// - State 1 (output): merge completed, output hash known
/// - State 2 (inputs): merge in progress, input hashes known
#[derive(Debug, Clone)]
pub enum PendingMergeState {
    /// State 1: merge completed, output bucket hash is known
    Output(Hash256),
    /// State 2: merge in progress, input curr/snap hashes are known
    Inputs { curr: Hash256, snap: Hash256 },
}

impl std::fmt::Debug for PendingMerge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PendingMerge::InMemory(b) => f
                .debug_struct("InMemory")
                .field("hash", &b.hash().to_hex())
                .finish(),
            PendingMerge::Async(h) => f.debug_struct("Async").field("level", &h.level).finish(),
        }
    }
}

impl PendingMerge {
    /// Get the hash of the pending merge result.
    ///
    /// For InMemory, returns the bucket hash directly.
    /// For Async, returns the cached result hash if resolved, otherwise returns a placeholder.
    pub fn hash(&self) -> Hash256 {
        match self {
            PendingMerge::InMemory(bucket) => bucket.hash(),
            PendingMerge::Async(handle) => {
                // If we have a cached result, return its hash
                if let Some(ref bucket) = handle.result {
                    bucket.hash()
                } else {
                    // Return zero hash to indicate unresolved async merge
                    Hash256::default()
                }
            }
        }
    }
}

/// Optional BucketList-level context passed to level merges.
///
/// Groups the merge behavior flags and shared resources that the
/// `BucketList` owns and threads into `BucketLevel::prepare_with_normalization`
/// and `AsyncMergeHandle::start_merge`.
pub(crate) struct MergeContext<'a> {
    /// Whether to keep dead (tombstone) entries in the merge output.
    pub keep_dead_entries: bool,
    /// If true, convert INIT entries to LIVE entries during the merge.
    /// Should ALWAYS be false in production; exists for test compatibility.
    pub normalize_init: bool,
    /// If true, use an empty bucket instead of the level's actual curr.
    pub use_empty_curr: bool,
    pub bucket_dir: Option<&'a std::path::Path>,
    pub merge_map: Option<&'a std::sync::Arc<std::sync::RwLock<BucketMergeMap>>>,
    pub merge_counters: Option<Arc<MergeCounters>>,
}

struct AsyncMergeRequest {
    curr: Arc<Bucket>,
    snap: Arc<Bucket>,
    keep_dead_entries: bool,
    protocol_version: u32,
    normalize_init: bool,
    shadow_buckets: Vec<Bucket>,
    level: usize,
    bucket_dir: Option<std::path::PathBuf>,
    counters: Option<Arc<MergeCounters>>,
}

struct AddBatchArgs {
    ledger_seq: u32,
    protocol_version: u32,
    bucket_list_type: BucketListType,
    init_entries: Vec<LedgerEntry>,
    live_entries: Vec<LedgerEntry>,
    dead_entries: Vec<LedgerKey>,
}

/// Handle to an asynchronous bucket merge running in a background thread.
///
/// The merge is started immediately when this handle is created, and runs
/// concurrently with other operations. Call `resolve()` to wait for completion.
pub struct AsyncMergeHandle {
    /// Oneshot channel to receive the merge result.
    receiver: Option<oneshot::Receiver<Result<Bucket>>>,
    /// The level this merge is for (for logging/debugging).
    level: usize,
    /// Cached result after resolution (allows multiple reads).
    result: Option<Arc<Bucket>>,
    /// Input bucket file paths that must not be deleted while merge is in progress.
    /// These paths are needed for garbage collection - we can't delete files that
    /// are being read by in-flight merges.
    input_file_paths: Vec<std::path::PathBuf>,
    /// Hash of the "curr" input bucket for this merge.
    /// Stored so we can serialize in-progress merges as HAS state 2 (input hashes),
    /// matching stellar-core FutureBucket's hasHashes() behavior.
    input_curr_hash: Hash256,
    /// Hash of the "snap" input bucket for this merge.
    input_snap_hash: Hash256,
    /// Merge key for recording completed merges in the BucketMergeMap.
    merge_key: MergeKey,
}

impl AsyncMergeHandle {
    /// Create a new async merge handle and start the merge in a background thread.
    ///
    /// Uses `tokio::task::spawn_blocking` to run the merge on tokio's blocking thread pool,
    /// which is properly sized and managed. This avoids creating unbounded OS threads and
    /// integrates well with the tokio runtime.
    ///
    /// # Panics
    ///
    /// Panics if called outside of a tokio runtime context. Tests should use `#[tokio::test(flavor = "multi_thread")]`.
    fn start_merge(request: AsyncMergeRequest) -> Self {
        let AsyncMergeRequest {
            curr,
            snap,
            keep_dead_entries,
            protocol_version,
            normalize_init,
            shadow_buckets,
            level,
            bucket_dir,
            counters,
        } = request;
        let (sender, receiver) = oneshot::channel();

        // Capture input hashes BEFORE the merge starts. These are needed for
        // HAS serialization: if the merge is still in progress when we persist
        // the HAS, we store these as state=2 (FB_HASH_INPUTS) so that
        // restart_merges_from_has can reconstruct the exact merge.
        let input_curr_hash = curr.hash();
        let input_snap_hash = snap.hash();

        // Capture input bucket file paths for garbage collection tracking.
        // These files must not be deleted while this merge is in progress.
        let mut input_file_paths = Vec::new();
        if let Some(path) = curr.backing_file_path() {
            input_file_paths.push(path.to_path_buf());
        }
        if let Some(path) = snap.backing_file_path() {
            input_file_paths.push(path.to_path_buf());
        }
        // Also track shadow bucket paths
        for shadow in &shadow_buckets {
            if let Some(path) = shadow.backing_file_path() {
                input_file_paths.push(path.to_path_buf());
            }
        }

        // Spawn the merge on tokio's blocking thread pool.
        // This is better than std::thread::spawn because:
        // 1. Uses a managed thread pool with appropriate sizing
        // 2. Avoids unbounded thread creation
        // 3. Integrates with tokio's shutdown handling
        tokio::task::spawn_blocking(move || {
            let start = std::time::Instant::now();
            tracing::debug!(
                level,
                disk_backed = bucket_dir.is_some(),
                "Background merge started"
            );

            let counters_ref = counters.as_deref();

            let result = if let Some(ref dir) = bucket_dir {
                // Disk-backed merge: write output to temp file, create DiskBacked bucket.
                // This keeps memory O(index_size) instead of O(data_size).
                let temp_path = temp_merge_path(dir);
                match merge_buckets_to_file_with_counters(
                    &curr,
                    &snap,
                    &temp_path,
                    keep_dead_entries,
                    protocol_version,
                    normalize_init,
                    counters_ref,
                ) {
                    Ok((hash, entry_count)) => {
                        if entry_count == 0 {
                            let _ = std::fs::remove_file(&temp_path);
                            Ok(Bucket::empty())
                        } else {
                            promote_temp_to_canonical(&temp_path, &dir, &hash, "start_merge")
                        }
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&temp_path);
                        Err(e)
                    }
                }
            } else {
                // In-memory merge (used in tests or when no bucket_dir is set)
                merge_buckets_with_options_and_shadows_and_counters(
                    &curr,
                    &snap,
                    keep_dead_entries,
                    protocol_version,
                    normalize_init,
                    &shadow_buckets,
                    counters_ref,
                )
            };

            let elapsed = start.elapsed();

            // Record merge timing in counters
            if let Some(ref c) = counters {
                c.record_merge_completed(elapsed.as_micros() as u64);
            }

            match &result {
                Ok(bucket) => {
                    tracing::debug!(
                        level,
                        duration_ms = elapsed.as_millis(),
                        result_hash = %bucket.hash().to_hex(),
                        result_entries = bucket.len(),
                        disk_backed = bucket_dir.is_some(),
                        "Background merge completed successfully"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        level,
                        duration_ms = elapsed.as_millis(),
                        error = %e,
                        "Background merge failed"
                    );
                }
            }

            // Send the result; ignore errors if receiver was dropped
            let _ = sender.send(result);
        });

        let merge_key = MergeKey::new(keep_dead_entries, input_curr_hash, input_snap_hash);

        Self {
            receiver: Some(receiver),
            level,
            result: None,
            input_file_paths,
            input_curr_hash,
            input_snap_hash,
            merge_key,
        }
    }

    /// Resolve the merge, blocking until complete if necessary.
    ///
    /// After calling this, the result is cached and can be retrieved multiple times.
    ///
    /// Uses `tokio::task::block_in_place` when called from within a tokio runtime
    /// to avoid the "cannot block from async context" panic, while still allowing
    /// synchronous use in the ledger close path.
    pub fn resolve(&mut self) -> Result<Arc<Bucket>> {
        if let Some(ref result) = self.result {
            return Ok(result.clone());
        }

        let receiver = self
            .receiver
            .take()
            .ok_or_else(|| BucketError::Merge("merge handle already consumed".to_string()))?;

        let start = std::time::Instant::now();

        // Use block_in_place to allow blocking from within an async context.
        // This moves the blocking operation to a blocking thread, avoiding the
        // "cannot block the current thread" panic when called from tokio tests
        // or async code paths.
        let bucket = tokio::task::block_in_place(|| {
            receiver
                .blocking_recv()
                .map_err(|_| BucketError::Merge("merge task was cancelled".to_string()))
        })??;

        let elapsed = start.elapsed();

        if elapsed.as_millis() > 10 {
            tracing::info!(
                level = self.level,
                wait_ms = elapsed.as_millis(),
                "Waited for background merge to complete"
            );
        }

        let bucket = Arc::new(bucket);
        self.result = Some(bucket.clone());
        Ok(bucket)
    }
}

/// A single level in the BucketList, containing `curr` and `snap` buckets.
///
/// Each level maintains two buckets:
/// - `curr`: Receives merged data from the level below when it spills
/// - `snap`: Previous `curr` that was "snapped" during a spill
///
/// The level also has a `next` pending merge used during merge operations to
/// stage the result before committing it to `curr`.
///
/// # Spill Behavior
///
/// When a level spills:
/// 1. The old `snap` is returned (flows to the next level)
/// 2. `curr` becomes the new `snap`
/// 3. `curr` is reset to empty (ready for new merges)
///
/// # Background Merging
///
/// For levels 1+, merges run asynchronously in background threads. When `prepare()`
/// is called, the merge is started immediately but returns without waiting. The
/// merge result is retrieved when `commit()` is called, blocking only if the merge
/// hasn't completed yet.
///
/// This matches stellar-core's FutureBucket design and allows merges for higher
/// levels (which take longer) to run concurrently with other ledger close operations.
#[derive(Debug)]
pub struct BucketLevel {
    /// The current bucket being filled with merged entries.
    pub curr: Arc<Bucket>,
    /// The snapshot bucket from the previous spill.
    pub snap: Arc<Bucket>,
    /// Pending merge result awaiting commit (replaces `curr` on commit).
    next: Option<PendingMerge>,
    /// The level number (0-10).
    level: usize,
}

impl BucketLevel {
    /// Create a new empty level.
    pub fn new(level: usize) -> Self {
        Self {
            curr: Arc::new(Bucket::empty()),
            snap: Arc::new(Bucket::empty()),
            next: None,
            level,
        }
    }

    /// Get the hash of this level: SHA256(curr_hash || snap_hash).
    ///
    /// This matches stellar-core's BucketLevel::getHash() implementation.
    pub fn hash(&self) -> Hash256 {
        let curr_hash = self.curr.hash();
        let snap_hash = self.snap.hash();

        // SHA256(curr_hash || snap_hash)
        let mut hasher = Sha256::new();
        hasher.update(curr_hash.as_bytes());
        hasher.update(snap_hash.as_bytes());
        let result = hasher.finalize();

        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&result);
        Hash256::from_bytes(bytes)
    }

    /// Set the curr bucket.
    pub fn set_curr(&mut self, bucket: Bucket) {
        self.curr = Arc::new(bucket);
    }

    /// Set the snap bucket.
    pub fn set_snap(&mut self, bucket: Bucket) {
        self.snap = Arc::new(bucket);
    }

    /// Get the level number.
    pub fn level_number(&self) -> usize {
        self.level
    }

    /// Promote the prepared bucket into curr, if any.
    ///
    /// For async merges, this will block until the merge completes.
    /// This matches stellar-core's BucketLevel::commit() behavior where
    /// merge failures are fatal (propagated as exceptions / releaseAssert).
    ///
    /// Returns the merge key and output hash for async merges that completed
    /// successfully, so the caller can record them in the BucketMergeMap.
    fn commit(&mut self) -> Result<Option<(MergeKey, Hash256)>> {
        if let Some(pending) = self.next.take() {
            match pending {
                PendingMerge::InMemory(bucket) => {
                    self.curr = Arc::new(bucket);
                    Ok(None)
                }
                PendingMerge::Async(mut handle) => {
                    let merge_key = handle.merge_key.clone();
                    let bucket = handle.resolve()?;
                    let output_hash = bucket.hash();
                    self.curr = bucket;
                    Ok(Some((merge_key, output_hash)))
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Check if there is an in-progress merge.
    pub fn has_in_progress_merge(&self) -> bool {
        self.next.is_some()
    }

    /// Get the full pending merge state for HAS serialization.
    ///
    /// Returns the state needed to serialize this level's `next` field in the
    /// History Archive State JSON, matching stellar-core FutureBucket serialization:
    ///
    /// - `None` → state 0 (clear): no pending merge
    /// - `Some(PendingMergeState::Output(hash))` → state 1: merge completed, output hash known
    /// - `Some(PendingMergeState::Inputs { curr, snap })` → state 2: merge in progress, input hashes known
    pub fn pending_merge_state(&self) -> Option<PendingMergeState> {
        match &self.next {
            None => None,
            Some(PendingMerge::InMemory(bucket)) => {
                // InMemory merges are always resolved; emit state 1 (output)
                let h = bucket.hash();
                if h.is_zero() {
                    None
                } else {
                    Some(PendingMergeState::Output(h))
                }
            }
            Some(PendingMerge::Async(handle)) => {
                if let Some(ref bucket) = handle.result {
                    // Async merge has resolved; emit state 1 (output)
                    let h = bucket.hash();
                    if h.is_zero() {
                        None
                    } else {
                        Some(PendingMergeState::Output(h))
                    }
                } else {
                    // Async merge still in progress; emit state 2 (input hashes)
                    Some(PendingMergeState::Inputs {
                        curr: handle.input_curr_hash,
                        snap: handle.input_snap_hash,
                    })
                }
            }
        }
    }

    /// Resolve any pending async merge without committing it.
    ///
    /// This ensures that if this level has an async merge in progress,
    /// we wait for it to complete and cache its result. This is necessary
    /// before cloning the bucket list, as unresolved async merges would be
    /// lost during cloning.
    ///
    /// Merge failures are propagated as errors (matching stellar-core's
    /// fatal behavior for merge failures).
    pub fn resolve_pending_merge(&mut self) -> Result<()> {
        if let Some(PendingMerge::Async(ref mut handle)) = self.next {
            if handle.result.is_none() {
                // Resolve the async merge, which caches the result in the handle
                handle.resolve()?;
            }
        }
        Ok(())
    }

    /// Get a reference to the next bucket if any (pending merge result).
    /// Used for lookups to check pending merges.
    ///
    /// Note: For async merges that haven't completed yet, this returns None.
    /// To get the result of an async merge, use commit() which will block if needed.
    pub fn next(&self) -> Option<&Bucket> {
        match &self.next {
            Some(PendingMerge::InMemory(bucket)) => Some(bucket),
            Some(PendingMerge::Async(handle)) => {
                // For async, only return if we have a cached result
                handle.result.as_ref().map(|arc| arc.as_ref())
            }
            None => None,
        }
    }

    /// Snap the current bucket to become the new snapshot.
    ///
    /// This implements the bucket list spill behavior (matches stellar-core BucketLevel::snap):
    /// - Sets snap = curr (old curr becomes the new snap)
    /// - Clears curr (ready for new entries)
    /// - Returns the NEW snap (old curr), which flows to the next level
    ///
    /// Note: Unlike commit(), snap() does NOT commit pending merges. In stellar-core,
    /// mNextCurr is a FutureBucket that stays pending until explicitly committed.
    fn snap(&mut self) -> Arc<Bucket> {
        // Move curr to snap, replacing curr with empty bucket
        let old_curr = std::mem::replace(&mut self.curr, Arc::new(Bucket::empty()));
        self.snap = old_curr;
        // Return the new snap (old curr) for merging into next level
        Arc::clone(&self.snap)
    }

    /// Prepare the next bucket for this level with explicit INIT normalization control.
    ///
    /// This merges the current bucket (self.curr) with the incoming bucket.
    /// The curr may be empty if this level was already snapped from processing
    /// higher levels first.
    ///
    /// Merge behavior flags and shared resources are bundled in [`MergeContext`].
    fn prepare_with_normalization(
        &mut self,
        protocol_version: u32,
        incoming: Arc<Bucket>,
        shadow_buckets: &[Bucket],
        ctx: MergeContext<'_>,
    ) -> Result<()> {
        if self.next.is_some() {
            return Err(BucketError::Merge(
                "bucket merge already in progress".to_string(),
            ));
        }

        // Choose curr or empty based on shouldMergeWithEmptyCurr
        let curr_for_merge: Arc<Bucket> = if ctx.use_empty_curr {
            tracing::debug!(
                level = self.level,
                "prepare_with_normalization: using EMPTY curr (shouldMergeWithEmptyCurr=true)"
            );
            Arc::new(Bucket::empty())
        } else {
            tracing::debug!(
                level = self.level,
                curr_hash = %self.curr.hash(),
                curr_entries = self.curr.len(),
                "prepare_with_normalization: using actual curr"
            );
            Arc::clone(&self.curr)
        };

        tracing::debug!(
            level = self.level,
            curr_for_merge_hash = %curr_for_merge.hash(),
            curr_for_merge_entries = curr_for_merge.len(),
            incoming_hash = %incoming.hash(),
            incoming_entries = incoming.len(),
            keep_dead_entries = ctx.keep_dead_entries,
            normalize_init = ctx.normalize_init,
            "prepare_with_normalization: about to merge"
        );

        // Check merge map for a cached result before starting a new merge.
        // If this exact merge (same inputs + keep_dead_entries) was previously
        // completed, reuse the output instead of re-computing.
        if let Some(merge_map) = ctx.merge_map {
            let curr_hash = curr_for_merge.hash();
            let snap_hash = incoming.hash();
            let key = MergeKey::new(ctx.keep_dead_entries, curr_hash, snap_hash);
            if let Some(output_hash) = merge_map.read().unwrap().get_output(&key).copied() {
                if !output_hash.is_zero() {
                    if let Some(dir) = ctx.bucket_dir {
                        let path = dir.join(canonical_bucket_filename(&output_hash));
                        if path.exists() {
                            match Bucket::from_xdr_file_disk_backed(&path) {
                                Ok(bucket) => {
                                    tracing::debug!(
                                        level = self.level,
                                        output_hash = %output_hash,
                                        "Reusing cached merge result from merge map"
                                    );
                                    self.next = Some(PendingMerge::InMemory(bucket));
                                    return Ok(());
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        level = self.level,
                                        error = %e,
                                        "Failed to load cached merge result, falling through to new merge"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        // For levels 1+, use async merging to run merges in background threads.
        // This matches stellar-core's FutureBucket design where merges for
        // higher levels (which have larger buckets) start immediately and run
        // concurrently with other operations. The merge result is retrieved when
        // commit() is called, blocking only if the merge hasn't finished yet.
        //
        // Level 0 uses synchronous in-memory merging (handled in prepare_first_level).
        if self.level >= 1 {
            let handle = AsyncMergeHandle::start_merge(AsyncMergeRequest {
                curr: curr_for_merge,
                snap: incoming,
                keep_dead_entries: ctx.keep_dead_entries,
                protocol_version,
                normalize_init: ctx.normalize_init,
                shadow_buckets: shadow_buckets.to_vec(),
                level: self.level,
                bucket_dir: ctx.bucket_dir.map(|p| p.to_path_buf()),
                counters: ctx.merge_counters,
            });
            self.next = Some(PendingMerge::Async(handle));
        } else {
            // Level 0 should use prepare_first_level, but if called here, do sync merge
            let merged = merge_buckets_with_options_and_shadows_and_counters(
                &curr_for_merge,
                &incoming,
                ctx.keep_dead_entries,
                protocol_version,
                ctx.normalize_init,
                shadow_buckets,
                ctx.merge_counters.as_deref(),
            )?;

            tracing::debug!(
                level = self.level,
                merged_hash = %merged.hash(),
                merged_entries = merged.len(),
                "prepare_with_normalization: merge complete"
            );

            self.next = Some(PendingMerge::InMemory(merged));
        }
        Ok(())
    }

    /// Prepare level 0 with in-memory optimization.
    ///
    /// This method is specifically for level 0 and uses the in-memory merge
    /// optimization when possible. It avoids disk I/O for reading entries
    /// and keeps entries in memory for subsequent fast merges.
    ///
    /// # Arguments
    ///
    /// * `protocol_version` - The protocol version for the output bucket
    /// * `incoming` - The new bucket to merge with curr (must have in-memory entries)
    ///
    /// # Returns
    ///
    /// The merged bucket with in-memory entries set for the next merge.
    fn prepare_first_level(&mut self, protocol_version: u32, incoming: Bucket) -> Result<()> {
        if self.level != 0 {
            return Err(BucketError::Merge(
                "prepare_first_level can only be called on level 0".to_string(),
            ));
        }

        if self.next.is_some() {
            return Err(BucketError::Merge(
                "bucket merge already in progress".to_string(),
            ));
        }

        // Check if we can use in-memory merge
        let can_use_in_memory_merge =
            self.curr.has_in_memory_entries() && incoming.has_in_memory_entries();

        let merged = if can_use_in_memory_merge {
            tracing::debug!(level = 0, "prepare_first_level: using in-memory merge path");
            merge_in_memory(&self.curr, &incoming, protocol_version)?
        } else {
            tracing::debug!(
                level = 0,
                curr_has_in_memory = self.curr.has_in_memory_entries(),
                incoming_has_in_memory = incoming.has_in_memory_entries(),
                "prepare_first_level: falling back to regular merge (in-memory entries not available)"
            );
            // Fall back to regular merge
            // Level 0 always keeps tombstones and never normalizes INIT entries
            merge_buckets_with_options_and_shadows(
                &self.curr,
                &incoming,
                true, // keep_dead_entries
                protocol_version,
                false, // normalize_init_entries
                &[],   // no shadow buckets at level 0
            )?
        };

        // If the merged bucket doesn't have in-memory entries but we want them
        // for the next merge, try to enable them
        let merged = if !merged.has_in_memory_entries() {
            // Get entries and create bucket with in-memory optimization
            let entries: Vec<BucketEntry> = merged.iter()?.collect::<crate::Result<Vec<_>>>()?;
            Bucket::from_sorted_entries_with_in_memory(entries)?
        } else {
            merged
        };

        // Level 0 uses synchronous in-memory merge
        self.next = Some(PendingMerge::InMemory(merged));
        Ok(())
    }

    /// Clear in-memory entries from curr and snap buckets.
    ///
    /// This should be called when entries from this level move to higher levels
    /// and no longer need to participate in fast in-memory merges.
    pub fn clear_in_memory_entries(&mut self) {
        Arc::make_mut(&mut self.curr).clear_in_memory_entries();
        Arc::make_mut(&mut self.snap).clear_in_memory_entries();
    }
}

impl Default for BucketLevel {
    fn default() -> Self {
        Self::new(0)
    }
}

impl Clone for BucketLevel {
    fn clone(&self) -> Self {
        // For the `next` field, we can only clone InMemory variants.
        // Async variants would need to be resolved first, but since Clone
        // takes &self (not &mut self), we can't resolve them here.
        // Instead, we only clone the cached result if available.
        let cloned_next = match &self.next {
            None => None,
            Some(PendingMerge::InMemory(bucket)) => Some(PendingMerge::InMemory(bucket.clone())),
            Some(PendingMerge::Async(handle)) => {
                // If the async merge has completed and we have a cached result,
                // clone it as InMemory. Otherwise, skip the pending merge.
                // This is safe because:
                // 1. Cloning is typically done for snapshotting state
                // 2. Pending merges shouldn't be part of canonical state
                if let Some(ref result) = handle.result {
                    Some(PendingMerge::InMemory((**result).clone()))
                } else {
                    // Async merge not yet resolved - skip it
                    // The caller should resolve merges before cloning if they need them
                    tracing::warn!(
                        level = self.level,
                        "Cloning BucketLevel with unresolved async merge - merge will be lost"
                    );
                    None
                }
            }
        };

        Self {
            curr: self.curr.clone(),
            snap: self.snap.clone(),
            next: cloned_next,
            level: self.level,
        }
    }
}

/// The complete BucketList structure representing all ledger state.
///
/// The BucketList is Stellar's canonical on-disk state representation. It contains
/// 11 levels of buckets that together hold every ledger entry in the network.
///
/// # Structure
///
/// Each level contains two buckets (`curr` and `snap`), and levels update at
/// different frequencies based on their position:
///
/// | Level | Spill Period | Typical Contents              |
/// |-------|--------------|-------------------------------|
/// | 0     | 2 ledgers    | Very recent entries           |
/// | 1     | 8 ledgers    | Recent entries                |
/// | 2     | 32 ledgers   | Moderately recent entries     |
/// | ...   | ...          | ...                           |
/// | 10    | Never        | Oldest, most stable entries   |
///
/// # Key Operations
///
/// - [`add_batch`](BucketList::add_batch): Add entries from a closed ledger
/// - [`get`](BucketList::get): Look up a ledger entry by key
/// - [`hash`](BucketList::hash): Compute the Merkle root hash
/// - [`scan_for_eviction_incremental`](BucketList::scan_for_eviction_incremental): Soroban eviction scan
///
/// # Thread Safety
///
/// BucketList is `Clone` but not `Send` or `Sync` by default. For concurrent
/// access, wrap in appropriate synchronization primitives.
pub struct BucketList {
    /// The 11 levels of the bucket list (indices 0-10).
    levels: Vec<BucketLevel>,
    /// The current ledger sequence number (last ledger added).
    ledger_seq: u32,
    /// Optional directory for writing merge output files.
    /// When set, merges at level 1+ write to disk instead of collecting in memory,
    /// reducing peak memory from O(data_size) to O(index_size).
    bucket_dir: Option<std::path::PathBuf>,
    /// Optional BucketListDB config for per-bucket cache initialization.
    /// When set, `add_batch()` calls `maybe_initialize_caches()` after updating levels.
    bucket_list_db_config: Option<BucketListDbConfig>,
    /// Merges completed during the last `add_batch_internal` call.
    /// The caller should drain these and record them in the BucketManager's
    /// merge map for deduplication.
    completed_merges: Vec<(MergeKey, Hash256)>,
    /// Optional reference to the merge map for checking cached merge results
    /// before starting new merges. Shared with BucketManager.
    merge_map: Option<std::sync::Arc<std::sync::RwLock<BucketMergeMap>>>,
    /// Counters for merge operations (shared across all merge calls).
    merge_counters: Arc<MergeCounters>,
    /// Background thread handle for async bucket persistence.
    /// Bounded to one concurrent write; the next add_batch waits for completion.
    pending_persist: Option<std::thread::JoinHandle<()>>,
}

/// Deduplicate ledger entries by key, keeping only the last occurrence.
/// This ensures that when the same entry is updated multiple times in a single
/// ledger, only the final state is included in the bucket.
///
/// Parity: stellar-core uses `releaseAssert` with `adjacent_find` to crash on
/// duplicates after sorting (`LiveBucket.cpp:414`). We keep the dedup behavior
/// for resilience but warn when duplicates are actually found, since their
/// presence indicates a bug in the entry-generation path.
fn deduplicate_entries(entries: Vec<LedgerEntry>) -> Vec<LedgerEntry> {
    let original_count = entries.len();

    // Use a HashMap to track the last position of each key
    let mut key_positions: HashMap<Vec<u8>, usize> = HashMap::new();

    // First pass: record the position of each key (later entries overwrite earlier ones)
    for (idx, entry) in entries.iter().enumerate() {
        let key = henyey_common::entry_to_key(entry);
        if let Ok(key_bytes) = key.to_xdr(Limits::none()) {
            key_positions.insert(key_bytes, idx);
        }
    }

    // Second pass: collect only entries at the recorded positions (final state of each key)
    let positions: HashSet<usize> = key_positions.values().copied().collect();
    let result: Vec<LedgerEntry> = entries
        .into_iter()
        .enumerate()
        .filter_map(|(idx, entry)| {
            if positions.contains(&idx) {
                Some(entry)
            } else {
                None
            }
        })
        .collect();

    let removed = original_count - result.len();
    if removed > 0 {
        tracing::warn!(
            removed,
            original_count,
            "deduplicate_entries removed duplicate bucket entries; \
             this indicates a bug in the entry-generation path"
        );
    }

    result
}

/// Perform a single bucket merge, writing to disk if a bucket directory is provided,
/// otherwise merging in memory. Used by `restart_merges_from_has` to run merges
/// concurrently via `spawn_blocking`.
fn perform_merge(
    input_curr: &Bucket,
    input_snap: &Bucket,
    bucket_dir: Option<&std::path::PathBuf>,
    keep_dead: bool,
    protocol_version: u32,
) -> Result<Bucket> {
    if let Some(dir) = bucket_dir {
        let temp_path = temp_merge_path(dir);
        let (hash, entry_count) = merge_buckets_to_file(
            input_curr,
            input_snap,
            &temp_path,
            keep_dead,
            protocol_version,
            false, // normalize_init = false
        )?;
        if entry_count == 0 {
            let _ = std::fs::remove_file(&temp_path);
            Ok(Bucket::empty())
        } else {
            promote_temp_to_canonical(&temp_path, dir, &hash, "perform_merge")
        }
    } else {
        merge_buckets_with_options_and_shadows(
            input_curr,
            input_snap,
            keep_dead,
            protocol_version,
            false, // normalize_init = false
            &[],   // no shadows for post-protocol-12
        )
    }
}

impl BucketList {
    /// Number of levels in the BucketList.
    pub const NUM_LEVELS: usize = BUCKET_LIST_LEVELS;

    /// Create a new empty BucketList.
    pub fn new() -> Self {
        let levels = (0..BUCKET_LIST_LEVELS).map(BucketLevel::new).collect();

        Self {
            levels,
            ledger_seq: 0,
            bucket_dir: None,
            bucket_list_db_config: None,
            completed_merges: Vec::new(),
            merge_map: None,
            merge_counters: Arc::new(MergeCounters::new()),
            pending_persist: None,
        }
    }

    /// Set the bucket directory for disk-backed merge output.
    ///
    /// When set, merges at level 1+ write output to this directory as temporary
    /// XDR files and create DiskBacked buckets, keeping memory O(index_size)
    /// instead of O(data_size). This is critical for mainnet where merge outputs
    /// at higher levels can be tens of GB.
    pub fn set_bucket_dir(&mut self, dir: std::path::PathBuf) {
        self.bucket_dir = Some(dir);
    }

    /// Set the merge map for checking cached merge results.
    ///
    /// When set, `prepare_with_normalization` checks the merge map before starting
    /// a new merge. If a matching merge was previously completed, the cached result
    /// is reused instead of re-computing the merge.
    pub fn set_merge_map(&mut self, merge_map: std::sync::Arc<std::sync::RwLock<BucketMergeMap>>) {
        self.merge_map = Some(merge_map);
    }

    /// Drain completed merge records from the last `add_batch` call.
    ///
    /// Returns merge keys and output hashes for all merges that completed
    /// during the last `add_batch_internal`. The caller should record these
    /// in the BucketManager's merge map for future deduplication.
    pub fn drain_completed_merges(&mut self) -> Vec<(MergeKey, Hash256)> {
        std::mem::take(&mut self.completed_merges)
    }

    /// Returns a reference to the merge counters.
    pub fn merge_counters(&self) -> &MergeCounters {
        &self.merge_counters
    }

    /// Resolve all pending async merges without committing them.
    ///
    /// This should be called before cloning a bucket list to ensure that all
    /// async merges are resolved and their results are cached, preventing data
    /// loss during cloning.
    pub fn resolve_all_pending_merges(&mut self) -> Result<()> {
        for level in &mut self.levels {
            level.resolve_pending_merge()?;
        }
        Ok(())
    }

    /// Get the hash of the entire BucketList.
    ///
    /// This is computed by hashing all level hashes together.
    pub fn hash(&self) -> Hash256 {
        let mut hasher = Sha256::new();

        for level in &self.levels {
            hasher.update(level.hash().as_bytes());
        }

        let result = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&result);
        Hash256::from_bytes(bytes)
    }

    /// Get the current ledger sequence.
    pub fn ledger_seq(&self) -> u32 {
        self.ledger_seq
    }

    /// Set the ledger sequence.
    ///
    /// This is used after restoring a bucket list from history archive hashes
    /// to set the correct ledger sequence. The `restore_from_hashes` method
    /// sets ledger_seq to 0, so callers must set it to the actual ledger
    /// sequence to ensure proper bucket list advancement behavior.
    pub fn set_ledger_seq(&mut self, ledger_seq: u32) {
        self.ledger_seq = ledger_seq;
    }

    /// Get a reference to a level.
    pub fn level(&self, idx: usize) -> Option<&BucketLevel> {
        self.levels.get(idx)
    }

    /// Get a mutable reference to a level.
    pub fn level_mut(&mut self, idx: usize) -> Option<&mut BucketLevel> {
        self.levels.get_mut(idx)
    }

    /// Get all levels.
    pub fn levels(&self) -> &[BucketLevel] {
        &self.levels
    }

    /// Get the hash of each level along with curr and snap bucket hashes.
    ///
    /// Returns an iterator of (level_index, level_hash, curr_hash, snap_hash).
    /// This is useful for debugging bucket list hash mismatches.
    pub fn level_hashes(&self) -> impl Iterator<Item = (usize, Hash256, Hash256, Hash256)> + '_ {
        self.levels
            .iter()
            .enumerate()
            .map(|(idx, level)| (idx, level.hash(), level.curr.hash(), level.snap.hash()))
    }

    /// Set the BucketListDB config for per-bucket cache initialization.
    ///
    /// When set, `add_batch()` calls `maybe_initialize_caches()` after updating
    /// levels, and per-bucket caches are proportionally sized based on
    /// `memory_for_caching_mb`.
    pub fn set_bucket_list_db_config(&mut self, config: BucketListDbConfig) {
        self.bucket_list_db_config = Some(config);
    }

    /// Returns the current BucketListDB config, if set.
    pub fn bucket_list_db_config(&self) -> Option<&BucketListDbConfig> {
        self.bucket_list_db_config.as_ref()
    }

    /// Sums entry counters across all buckets in the bucket list.
    ///
    /// Matches stellar-core's `LiveBucketList::sumBucketEntryCounters`.
    pub fn sum_bucket_entry_counters(&self) -> BucketEntryCounters {
        let mut counters = BucketEntryCounters::new();
        for level in &self.levels {
            for bucket in [&level.curr, &level.snap] {
                if !bucket.is_empty() {
                    if let Some(idx_counters) = bucket.entry_counters() {
                        counters.merge(idx_counters);
                    }
                }
            }
        }
        counters
    }

    /// Initializes per-bucket caches for all DiskIndex buckets.
    ///
    /// Matches stellar-core's `LiveBucketList::maybeInitializeCaches`.
    /// Each DiskIndex bucket gets a proportional share of the configured
    /// `memory_for_caching_mb` budget based on its account byte fraction.
    pub fn maybe_initialize_caches(&self) {
        let Some(config) = &self.bucket_list_db_config else {
            return;
        };
        if config.memory_for_caching_mb == 0 {
            return;
        }
        let counters = self.sum_bucket_entry_counters();
        let total_account_bytes =
            counters.size_for_type(stellar_xdr::curr::LedgerEntryType::Account);
        for level in &self.levels {
            for bucket in [&level.curr, &level.snap] {
                if !bucket.is_empty() {
                    bucket.maybe_initialize_cache(total_account_bytes, config);
                }
            }
        }
    }

    /// Returns aggregated cache statistics across all per-bucket caches.
    ///
    /// Sums hits/misses from all DiskIndex bucket caches and resets their counters.
    pub fn aggregate_cache_stats(&self) -> CacheStats {
        let mut total = CacheStats {
            entry_count: 0,
            size_bytes: 0,
            max_bytes: 0,
            max_entries: 0,
            hits: 0,
            misses: 0,
            hit_rate: 0.0,
            active: false,
        };
        for level in &self.levels {
            for bucket in [&level.curr, &level.snap] {
                if !bucket.is_empty() {
                    if let Some(stats) = bucket.cache_stats() {
                        total.entry_count += stats.entry_count;
                        total.size_bytes += stats.size_bytes;
                        total.max_bytes += stats.max_bytes;
                        total.max_entries += stats.max_entries;
                        total.hits += stats.hits;
                        total.misses += stats.misses;
                        if stats.active {
                            total.active = true;
                        }
                        bucket.reset_cache_counters();
                    }
                }
            }
        }
        let total_requests = total.hits + total.misses;
        if total_requests > 0 {
            total.hit_rate = total.hits as f64 / total_requests as f64;
        }
        total
    }

    /// Estimate total heap bytes across all bucket levels (indexes + caches).
    pub fn estimate_heap_bytes(&self) -> usize {
        let mut total = 0;
        for level in &self.levels {
            total += level.curr.estimate_heap_bytes();
            total += level.snap.estimate_heap_bytes();
        }
        total
    }

    /// Total mmap'd (file-backed) bytes across all bucket levels.
    pub fn mmap_bytes(&self) -> usize {
        let mut total = 0;
        for level in &self.levels {
            total += level.curr.mmap_bytes();
            total += level.snap.mmap_bytes();
        }
        total
    }

    /// Total cache bytes across all bucket levels.
    pub fn cache_bytes(&self) -> usize {
        let mut total = 0;
        for level in &self.levels {
            for bucket in [&level.curr, &level.snap] {
                if let Some(stats) = bucket.cache_stats() {
                    total += stats.size_bytes;
                }
            }
        }
        total
    }

    /// Look up an entry by its key.
    ///
    /// Searches from the newest (level 0) to oldest levels.
    /// Returns the first matching entry found, or None if not found.
    pub fn get(&self, key: &LedgerKey) -> Result<Option<LedgerEntry>> {
        self.get_with_debug(key, false)
    }

    /// Look up an entry by its key with optional debug tracing.
    ///
    /// Per-bucket caches (inside each DiskBucket) handle caching transparently;
    /// no global cache logic is needed here.
    pub fn get_with_debug(&self, key: &LedgerKey, debug: bool) -> Result<Option<LedgerEntry>> {
        // Search from newest to oldest (curr then snap). Pending merges (next)
        // are not part of the bucket list state yet.
        for (level_idx, level) in self.levels.iter().enumerate() {
            // Check curr bucket first
            if let Some(entry) = level.curr.get(key)? {
                if debug {
                    tracing::info!(
                        level = level_idx,
                        bucket = "curr",
                        entry_type = ?std::mem::discriminant(&entry),
                        "Found entry in bucket list"
                    );
                }
                return match entry {
                    BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) => Ok(Some(e.clone())),
                    BucketEntry::Deadentry(_) => Ok(None), // Entry is deleted
                    BucketEntry::Metaentry(_) => continue,
                };
            }

            // Then check snap bucket
            if let Some(entry) = level.snap.get(key)? {
                if debug {
                    tracing::info!(
                        level = level_idx,
                        bucket = "snap",
                        entry_type = ?std::mem::discriminant(&entry),
                        "Found entry in bucket list"
                    );
                }
                return match entry {
                    BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) => Ok(Some(e.clone())),
                    BucketEntry::Deadentry(_) => Ok(None), // Entry is deleted
                    BucketEntry::Metaentry(_) => continue,
                };
            }
        }

        if debug {
            tracing::info!("Entry not found in any bucket");
        }
        Ok(None)
    }

    /// Debug method to find ALL occurrences of a key across all buckets.
    /// Returns a list of (level, bucket_type, entry) for each occurrence.
    /// Order: curr (newest) → snap (oldest) within each level.
    pub fn find_all_occurrences(
        &self,
        key: &LedgerKey,
    ) -> Result<Vec<(usize, &'static str, BucketEntry)>> {
        let mut results = Vec::new();

        for (level_idx, level) in self.levels.iter().enumerate() {
            if let Some(entry) = level.curr.get(key)? {
                results.push((level_idx, "curr", entry));
            }
            if let Some(entry) = level.snap.get(key)? {
                results.push((level_idx, "snap", entry));
            }
        }

        Ok(results)
    }

    /// Returns a streaming iterator over all live entries.
    ///
    /// This is a memory-efficient alternative to [`live_entries()`](Self::live_entries)
    /// that avoids materializing all entries into a `Vec`. It uses `HashSet<LedgerKey>`
    /// for deduplication, matching stellar-core's approach.
    ///
    /// # Memory Efficiency
    ///
    /// For mainnet scale (~60M entries):
    /// - `live_entries()`: ~52 GB (full entry Vec + serialized key HashSet)
    /// - `live_entries_iter()`: ~8.6 GB (LedgerKey HashSet only)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let bucket_list = BucketList::new();
    /// // ... populate bucket list ...
    ///
    /// for entry_result in bucket_list.live_entries_iter() {
    ///     let entry = entry_result?;
    ///     // Process entry immediately
    /// }
    /// ```
    pub fn live_entries_iter(&self) -> LiveEntriesIterator<'_> {
        LiveEntriesIterator::new(self)
    }

    /// Scan for entries of a specific type with per-type deduplication.
    ///
    /// This iterates through all buckets (level 0 to level 10, curr then snap)
    /// and invokes the callback for each live/init entry matching the specified type.
    /// Dead entries shadow older live entries with the same key.
    ///
    /// The dedup set is scoped to this single scan, so memory usage is proportional
    /// to the number of unique keys of the requested type (not all types combined).
    /// For mainnet, this means ~240 MB peak (for ContractData with ~1.68M keys)
    /// instead of ~8.6 GB (for all 60M keys across all types).
    ///
    /// # Returns
    ///
    /// `true` if iteration completed, `false` if stopped early by callback.
    pub fn scan_for_entries_of_type<F>(
        &self,
        entry_type: stellar_xdr::curr::LedgerEntryType,
        callback: F,
    ) -> Result<bool>
    where
        F: FnMut(&BucketEntry) -> bool,
    {
        self.scan_for_entries_of_types(&[entry_type], callback)
    }

    /// Scan the bucket list for live entries matching ANY of the given types.
    ///
    /// This is similar to `scan_for_entries_of_type` but accepts multiple types and
    /// performs a single pass over the bucket list, avoiding redundant I/O when multiple
    /// types need to be loaded together (e.g., ContractCode + ContractData + TTL +
    /// ConfigSetting for Soroban state initialization).
    ///
    /// A single `HashSet<LedgerKey>` is used for deduplication across all requested types.
    /// This is safe because `LedgerKey` is a discriminated union — keys of different types
    /// never collide.
    ///
    /// # Memory
    ///
    /// The dedup set holds keys for ALL requested types combined. For Soroban init
    /// (ContractCode + ContractData + TTL + ConfigSetting), this is ~3.66M keys (~480 MB)
    /// on mainnet.
    ///
    /// # Returns
    ///
    /// `true` if iteration completed, `false` if stopped early by callback.
    pub fn scan_for_entries_of_types<F>(
        &self,
        entry_types: &[stellar_xdr::curr::LedgerEntryType],
        mut callback: F,
    ) -> Result<bool>
    where
        F: FnMut(&BucketEntry) -> bool,
    {
        use stellar_xdr::curr::LedgerKey;

        let type_set: HashSet<stellar_xdr::curr::LedgerEntryType> =
            entry_types.iter().copied().collect();
        let mut seen_keys: HashSet<LedgerKey> = HashSet::new();

        for level in &self.levels {
            for bucket in [&*level.curr, &*level.snap] {
                for entry_result in bucket.iter()? {
                    let entry = entry_result?;
                    if let Some(key) = entry.key() {
                        if seen_keys.contains(&key) {
                            continue;
                        }

                        let entry_type = match &entry {
                            BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) => {
                                crate::entry::ledger_entry_data_type(&e.data)
                            }
                            BucketEntry::Deadentry(k) => crate::entry::ledger_key_type(k),
                            BucketEntry::Metaentry(_) => continue,
                        };

                        if type_set.contains(&entry_type) {
                            seen_keys.insert(key);

                            if !entry.is_dead() && !callback(&entry) {
                                return Ok(false);
                            }
                        }
                    }
                }
            }
        }
        Ok(true)
    }

    /// Return all live entries as of the current bucket list state.
    ///
    /// # Deprecation
    ///
    /// This method materializes all entries into memory, which is problematic
    /// for mainnet scale (~60M entries = ~52 GB RAM). Prefer using
    /// [`live_entries_iter()`](Self::live_entries_iter) for memory-efficient
    /// streaming iteration.
    #[deprecated(
        since = "0.2.0",
        note = "Use live_entries_iter() for memory-efficient streaming iteration"
    )]
    pub fn live_entries(&self) -> Result<Vec<LedgerEntry>> {
        let mut seen: HashSet<Vec<u8>> = HashSet::new();
        let mut entries = Vec::new();

        for level in &self.levels {
            // Collect buckets to iterate: curr (newest), snap (oldest)
            // The order matters because first occurrence shadows later ones.
            let buckets: [&Bucket; 2] = [&*level.curr, &*level.snap];

            for bucket in buckets {
                for entry_result in bucket.iter()? {
                    let entry = entry_result?;
                    match entry {
                        BucketEntry::Liveentry(live) | BucketEntry::Initentry(live) => {
                            let key = henyey_common::entry_to_key(&live);
                            let key_bytes = key.to_xdr(Limits::none()).map_err(|e| {
                                BucketError::Serialization(format!(
                                    "failed to serialize ledger key: {}",
                                    e
                                ))
                            })?;
                            if seen.insert(key_bytes) {
                                entries.push(live);
                            }
                        }
                        BucketEntry::Deadentry(dead) => {
                            let key_bytes = dead.to_xdr(Limits::none()).map_err(|e| {
                                BucketError::Serialization(format!(
                                    "failed to serialize ledger key: {}",
                                    e
                                ))
                            })?;
                            seen.insert(key_bytes);
                        }
                        BucketEntry::Metaentry(_) => {}
                    }
                }
            }
        }

        Ok(entries)
    }

    /// Check if an entry exists (is live) for the given key.
    pub fn contains(&self, key: &LedgerKey) -> Result<bool> {
        Ok(self.get(key)?.is_some())
    }

    /// Add ledger entries from a newly closed ledger.
    ///
    /// This mirrors stellar-core's bucket list update pipeline, preparing
    /// merges on spill boundaries and committing prior merges as needed.
    pub fn add_batch(
        &mut self,
        ledger_seq: u32,
        protocol_version: u32,
        bucket_list_type: BucketListType,
        init_entries: Vec<LedgerEntry>,
        live_entries: Vec<LedgerEntry>,
        dead_entries: Vec<LedgerKey>,
    ) -> Result<()> {
        self.add_batch_impl(
            AddBatchArgs {
                ledger_seq,
                protocol_version,
                bucket_list_type,
                init_entries,
                live_entries,
                dead_entries,
            },
            false,
        )
    }

    /// Like `add_batch`, but skips deduplication when entries are known to be
    /// unique (e.g. from a coalesced `LedgerDelta`). Saves ~200ms on 100K entries.
    pub fn add_batch_unique(
        &mut self,
        ledger_seq: u32,
        protocol_version: u32,
        bucket_list_type: BucketListType,
        init_entries: Vec<LedgerEntry>,
        live_entries: Vec<LedgerEntry>,
        dead_entries: Vec<LedgerKey>,
    ) -> Result<()> {
        self.add_batch_impl(
            AddBatchArgs {
                ledger_seq,
                protocol_version,
                bucket_list_type,
                init_entries,
                live_entries,
                dead_entries,
            },
            true,
        )
    }

    fn add_batch_impl(&mut self, args: AddBatchArgs, skip_dedup: bool) -> Result<()> {
        let AddBatchArgs {
            ledger_seq,
            protocol_version,
            bucket_list_type,
            init_entries,
            live_entries,
            dead_entries,
        } = args;
        let add_batch_start = std::time::Instant::now();
        let use_init = protocol_version_starts_from(protocol_version, ProtocolVersion::V11);

        let mut entries: Vec<BucketEntry> = Vec::new();

        if use_init {
            let mut meta = BucketMetadata {
                ledger_version: protocol_version,
                ext: BucketMetadataExt::V0,
            };
            if protocol_version_starts_from(protocol_version, ProtocolVersion::V23) {
                meta.ext = BucketMetadataExt::V1(bucket_list_type);
            }
            entries.push(BucketEntry::Metaentry(meta));
        }

        let dedup_start = std::time::Instant::now();
        if skip_dedup {
            // Entries from a coalesced delta are already unique per key.
            if use_init {
                entries.extend(init_entries.into_iter().map(BucketEntry::Initentry));
            } else {
                entries.extend(init_entries.into_iter().map(BucketEntry::Liveentry));
            }
            entries.extend(live_entries.into_iter().map(BucketEntry::Liveentry));
            entries.extend(dead_entries.into_iter().map(BucketEntry::Deadentry));
        } else {
            // Deduplicate init_entries - keep only the last occurrence of each key
            // This handles the case where the same entry is created and updated in the same ledger
            let dedup_init = deduplicate_entries(init_entries);
            if use_init {
                entries.extend(dedup_init.into_iter().map(BucketEntry::Initentry));
            } else {
                entries.extend(dedup_init.into_iter().map(BucketEntry::Liveentry));
            }

            // Deduplicate live_entries - keep only the last occurrence of each key
            // This handles the case where the same entry is updated multiple times in the same ledger
            let dedup_live = deduplicate_entries(live_entries);
            entries.extend(dedup_live.into_iter().map(BucketEntry::Liveentry));

            // Deduplicate dead_entries - keep only unique keys
            let mut seen_dead: HashSet<Vec<u8>> = HashSet::new();
            let dedup_dead: Vec<LedgerKey> = dead_entries
                .into_iter()
                .filter(|key| {
                    if let Ok(key_bytes) = key.to_xdr(Limits::none()) {
                        seen_dead.insert(key_bytes)
                    } else {
                        true
                    }
                })
                .collect();
            entries.extend(dedup_dead.into_iter().map(BucketEntry::Deadentry));
        }
        let dedup_us = dedup_start.elapsed().as_micros() as u64;

        // Create the new bucket with in-memory entries for level 0 optimization.
        // We use fresh_in_memory_only() which skips hash computation because:
        // 1. This bucket will be immediately merged with level 0 curr
        // 2. Only the merged result's hash matters for the bucket list
        // 3. Skipping hash computation saves ~50% of the bucket update time
        // This matches stellar-core's freshInMemoryOnly optimization.
        //
        // No global cache write-through is needed: new entries go to level 0
        // (always InMemory), and lookups check level 0 first, so stale
        // deeper-level per-bucket caches don't cause incorrect results.
        let sort_start = std::time::Instant::now();
        let new_bucket = Bucket::fresh_in_memory_only({
            let mut e = entries;
            // Use sort_by_cached_key to avoid repeated LedgerKey allocation.
            // key() clones AccountId/Asset/etc on every call; with O(n log n)
            // comparisons that's millions of allocations. Caching the key once
            // per entry reduces it to O(n).
            e.sort_by_cached_key(|entry| entry.key());
            e
        });
        let sort_us = sort_start.elapsed().as_micros() as u64;

        let internal_start = std::time::Instant::now();
        self.add_batch_internal(ledger_seq, protocol_version, new_bucket)?;
        let internal_us = internal_start.elapsed().as_micros() as u64;
        self.ledger_seq = ledger_seq;

        // Initialize per-bucket caches for any new DiskIndex buckets
        self.maybe_initialize_caches();

        let total_us = add_batch_start.elapsed().as_micros() as u64;
        tracing::debug!(
            ledger_seq,
            total_us,
            dedup_us,
            sort_us,
            internal_us,
            "PROFILE add_batch"
        );

        Ok(())
    }

    fn add_batch_internal(
        &mut self,
        ledger_seq: u32,
        protocol_version: u32,
        new_bucket: Bucket,
    ) -> Result<()> {
        if ledger_seq == 0 {
            return Err(BucketError::Merge(
                "ledger sequence must be > 0".to_string(),
            ));
        }

        // Clear completed merges from previous call
        self.completed_merges.clear();

        // Step 1: Process spills from highest level down to level 1
        // This matches stellar-core's BucketListBase::addBatchInternal
        //
        // The key insight is that snap() moves curr→snap and returns the NEW snap
        // (which is the old curr). This is the bucket that flows to the next level.
        //
        // By processing from highest to lowest, we ensure each level's curr is
        // available to be snapped before any modifications occur.
        //
        // When multiple levels spill at once (e.g., at checkpoint boundaries),
        // entries cascade up through the levels correctly.

        for i in (1..BUCKET_LIST_LEVELS).rev() {
            if Self::level_should_spill(ledger_seq, i - 1) {
                // Snap level i-1: moves curr→snap, returns the NEW snap (old curr)
                // This is the bucket that flows to level i
                let mut spilling_snap = self.levels[i - 1].snap();

                // Clear in-memory entries when buckets move beyond level 0.
                // Level 0 uses in-memory entries for fast merging, but once a bucket
                // moves to level 1+, we should release this memory since the bucket
                // is already persisted to disk and the in-memory entries are redundant.
                // This prevents a memory leak where Arc<Vec<BucketEntry>> references
                // would accumulate across bucket list generations.
                if i - 1 == 0 {
                    // Clear in-memory entries from the bucket flowing to level 1
                    Arc::make_mut(&mut spilling_snap).clear_in_memory_entries();
                    // Also clear from the snap position at level 0
                    Arc::make_mut(&mut self.levels[0].snap).clear_in_memory_entries();
                }

                tracing::debug!(
                    level = i - 1,
                    spilling_snap_hash = %spilling_snap.hash(),
                    new_snap_hash = %self.levels[i - 1].snap.hash(),
                    "Level snapped"
                );

                // Commit any pending merge at level i (promotes next→curr)
                let pre_commit_curr_hash = self.levels[i].curr.hash();
                let pre_commit_snap_hash = self.levels[i].snap.hash();
                let pre_commit_next_hash = self.levels[i]
                    .next
                    .as_ref()
                    .map(|b| b.hash().to_hex())
                    .unwrap_or_else(|| "None".to_string());

                // Record completed merges for deduplication
                if let Some((merge_key, output_hash)) = self.levels[i].commit()? {
                    self.completed_merges.push((merge_key, output_hash));
                }

                let post_commit_curr_hash = self.levels[i].curr.hash();
                tracing::debug!(
                    ledger = ledger_seq,
                    level = i,
                    pre_commit_curr = %pre_commit_curr_hash.to_hex(),
                    pre_commit_snap = %pre_commit_snap_hash.to_hex(),
                    pre_commit_next = %pre_commit_next_hash,
                    post_commit_curr = %post_commit_curr_hash.to_hex(),
                    "Level commit step"
                );

                // Prepare level i: merge curr with the spilling_snap from level i-1
                let keep_dead = Self::keep_tombstone_entries(i);
                let normalize_init = false; // Never normalize INIT to LIVE during merges
                let use_empty_curr = Self::should_merge_with_empty_curr(ledger_seq, i);
                let shadow_buckets =
                    if protocol_version_is_before(protocol_version, ProtocolVersion::V12) {
                        let mut shadows = Vec::new();
                        for level in self.levels.iter().take(i - 1) {
                            shadows.push((*level.curr).clone());
                            shadows.push((*level.snap).clone());
                        }
                        shadows
                    } else {
                        Vec::new()
                    };
                self.levels[i].prepare_with_normalization(
                    protocol_version,
                    Arc::clone(&spilling_snap),
                    &shadow_buckets,
                    MergeContext {
                        keep_dead_entries: keep_dead,
                        normalize_init,
                        use_empty_curr,
                        bucket_dir: self.bucket_dir.as_deref(),
                        merge_map: self.merge_map.as_ref(),
                        merge_counters: Some(Arc::clone(&self.merge_counters)),
                    },
                )?;

                let post_prepare_next_hash = self.levels[i]
                    .next
                    .as_ref()
                    .map(|b| b.hash().to_hex())
                    .unwrap_or_else(|| "None".to_string());
                tracing::debug!(
                    ledger = ledger_seq,
                    level = i,
                    use_empty_curr = use_empty_curr,
                    spilling_snap_hash = %spilling_snap.hash().to_hex(),
                    post_prepare_next = %post_prepare_next_hash,
                    post_prepare_curr = %self.levels[i].curr.hash().to_hex(),
                    post_prepare_snap = %self.levels[i].snap.hash().to_hex(),
                    "Level prepare step"
                );
            }
        }

        // Step 2: Apply new entries to level 0
        // Use the in-memory optimization for level 0
        // This avoids disk I/O for level 0 merges which happen frequently
        self.levels[0].prepare_first_level(protocol_version, new_bucket)?;
        // Level 0 commit doesn't produce merge keys (in-memory merges)
        self.levels[0].commit()?;

        // Ensure all curr/snap buckets have a permanent file on disk so that
        // restart recovery can locate them by hash.  Level 0 uses an in-memory
        // merge whose result has no backing file; writing it here means the
        // persisted HAS always references files that exist.
        //
        // Optimization: persist in a background thread so the critical path
        // doesn't block on disk I/O. The next add_batch_internal call will
        // wait for this thread to complete (via join) before starting new
        // persistence work, bounding concurrency to one background write.
        if let Some(ref dir) = self.bucket_dir {
            // Wait for any previous background persist to complete
            if let Some(handle) = self.pending_persist.take() {
                handle.join().ok();
            }

            let mut buckets_to_persist: Vec<(Arc<Bucket>, std::path::PathBuf)> = Vec::new();
            for level in &self.levels {
                for bucket_ref in [&level.curr, &level.snap] {
                    if bucket_ref.backing_file_path().is_none() && !bucket_ref.hash().is_zero() {
                        let permanent = dir.join(canonical_bucket_filename(&bucket_ref.hash()));
                        if !permanent.exists() {
                            buckets_to_persist.push((Arc::clone(bucket_ref), permanent));
                        }
                    }
                }
            }
            if !buckets_to_persist.is_empty() {
                self.pending_persist = Some(std::thread::spawn(move || {
                    for (bucket, path) in buckets_to_persist {
                        if let Err(e) = bucket.save_to_xdr_file(&path) {
                            tracing::warn!(
                                error = %e,
                                hash = %bucket.hash().to_hex(),
                                "Failed to persist in-memory bucket to disk"
                            );
                        }
                    }
                }));
            }
        }
        Ok(())
    }

    /// Advance the bucket list from its current ledger to a target ledger by
    /// applying empty batches for all intermediate ledgers.
    ///
    /// This is required because the bucket list merge algorithm depends on being
    /// called for every ledger in sequence. The spill boundaries and merge timing
    /// are determined by the ledger sequence number.
    ///
    /// # Arguments
    ///
    /// * `target_ledger` - The ledger to advance to (exclusive of actual changes)
    /// * `protocol_version` - Protocol version for empty batches
    /// * `bucket_list_type` - Type of bucket list (Live)
    ///
    /// # Returns
    ///
    /// Ok(()) if successful, or an error if the target is not greater than current.
    pub fn advance_to_ledger(
        &mut self,
        target_ledger: u32,
        protocol_version: u32,
        bucket_list_type: BucketListType,
    ) -> Result<()> {
        let current = self.ledger_seq;
        if target_ledger <= current {
            // Nothing to do - we're already at or past this ledger
            return Ok(());
        }

        // Apply empty batches for each intermediate ledger
        // This maintains the correct merge timing in the bucket list
        tracing::info!(
            current = current,
            target_ledger = target_ledger,
            count = target_ledger - current - 1,
            "advance_to_ledger: applying empty batches"
        );
        for seq in (current + 1)..target_ledger {
            tracing::debug!(
                from_ledger = current,
                to_ledger = target_ledger,
                current_seq = seq,
                "Advancing bucket list through empty ledger"
            );
            self.add_batch(
                seq,
                protocol_version,
                bucket_list_type,
                Vec::new(), // empty init
                Vec::new(), // empty live
                Vec::new(), // empty dead
            )?;
        }
        tracing::info!(
            current = current,
            target_ledger = target_ledger,
            "advance_to_ledger: completed"
        );

        Ok(())
    }

    /// Returns true if a level should spill at a given ledger.
    /// Delegates to [`bl_level_should_spill`].
    pub fn level_should_spill(ledger_seq: u32, level: usize) -> bool {
        bl_level_should_spill(ledger_seq, level, BUCKET_LIST_LEVELS)
    }

    fn keep_tombstone_entries(level: usize) -> bool {
        bl_keep_tombstone_entries(level, BUCKET_LIST_LEVELS)
    }

    fn should_merge_with_empty_curr(ledger_seq: u32, level: usize) -> bool {
        bl_should_merge_with_empty_curr(ledger_seq, level, BUCKET_LIST_LEVELS)
    }

    /// Get all hashes in the bucket list (for serialization).
    pub fn all_bucket_hashes(&self) -> Vec<Hash256> {
        let mut hashes = Vec::with_capacity(BUCKET_LIST_LEVELS * 2);
        for level in &self.levels {
            hashes.push(level.curr.hash());
            hashes.push(level.snap.hash());
        }
        hashes
    }

    /// Get all bucket hashes referenced by the bucket list, including pending merge
    /// inputs and outputs. This is used for garbage collection — any bucket files not
    /// in this set can be safely deleted.
    ///
    /// Matches stellar-core's `getBucketListReferencedBuckets()` which includes
    /// `level.getNext().getHashes()` (both merge inputs and outputs from FutureBucket).
    pub fn all_referenced_hashes(&self) -> Vec<Hash256> {
        let mut hashes = Vec::with_capacity(BUCKET_LIST_LEVELS * 4);
        for level in &self.levels {
            hashes.push(level.curr.hash());
            hashes.push(level.snap.hash());
            // Include pending merge hashes (inputs + output) to prevent
            // cleanup from deleting files still being read by in-flight merges.
            if let Some(ref pending) = level.next {
                match pending {
                    PendingMerge::InMemory(bucket) => {
                        hashes.push(bucket.hash());
                    }
                    PendingMerge::Async(handle) => {
                        hashes.push(handle.input_curr_hash);
                        hashes.push(handle.input_snap_hash);
                        if let Some(ref result) = handle.result {
                            hashes.push(result.hash());
                        }
                    }
                }
            }
        }
        hashes
    }

    /// Get all file paths referenced by disk-backed buckets in this bucket list.
    ///
    /// This includes:
    /// - curr and snap buckets for all levels
    /// - pending merge outputs that are disk-backed
    /// - input buckets for in-flight async merges (critical for correctness!)
    ///
    /// This is used for garbage collection - any files in the bucket directory
    /// that aren't in this set can be safely deleted.
    pub fn referenced_file_paths(&self) -> std::collections::HashSet<std::path::PathBuf> {
        let mut paths = std::collections::HashSet::new();

        for level in &self.levels {
            // Add curr bucket's backing file if disk-backed
            if let Some(path) = level.curr.backing_file_path() {
                paths.insert(path.to_path_buf());
            }
            // Add snap bucket's backing file if disk-backed
            if let Some(path) = level.snap.backing_file_path() {
                paths.insert(path.to_path_buf());
            }
            // Add pending merge files
            if let Some(ref pending) = level.next {
                match pending {
                    PendingMerge::InMemory(bucket) => {
                        // Add the output bucket's backing file if disk-backed
                        if let Some(path) = bucket.backing_file_path() {
                            paths.insert(path.to_path_buf());
                        }
                    }
                    PendingMerge::Async(handle) => {
                        // Add input bucket files that the merge is reading from.
                        // These MUST NOT be deleted while the merge is in progress!
                        for path in &handle.input_file_paths {
                            paths.insert(path.clone());
                        }
                        // If the merge has completed, also add the result's backing file
                        if let Some(ref result) = handle.result {
                            if let Some(path) = result.backing_file_path() {
                                paths.insert(path.to_path_buf());
                            }
                        }
                    }
                }
            }
        }

        paths
    }

    /// Restore a bucket list from hashes and a bucket lookup function.
    ///
    /// This is a convenience wrapper around [`Self::restore_from_has`] for cases where
    /// you only have bucket hashes without HAS next state information. It assumes
    /// no pending merges (state=0 for all levels), which is the common case at
    /// checkpoints.
    ///
    /// # Arguments
    ///
    /// * `hashes` - Flat array of bucket hashes (curr, snap for each level, 22 total)
    /// * `load_bucket` - Function to load a bucket by hash
    pub fn restore_from_hashes<F>(hashes: &[Hash256], load_bucket: F) -> Result<Self>
    where
        F: FnMut(&Hash256) -> Result<Bucket>,
    {
        if hashes.len() != BUCKET_LIST_LEVELS * 2 {
            return Err(BucketError::Serialization(format!(
                "Expected {} bucket hashes, got {}",
                BUCKET_LIST_LEVELS * 2,
                hashes.len()
            )));
        }

        // Convert flat array to (curr, snap) pairs
        let pairs: Vec<(Hash256, Hash256)> =
            hashes.chunks(2).map(|chunk| (chunk[0], chunk[1])).collect();

        // Use default next states (all state=0, no pending merges)
        let next_states = vec![HasNextState::default(); BUCKET_LIST_LEVELS];

        Self::restore_from_has(&pairs, &next_states, load_bucket)
    }

    /// Parallel variant of [`Self::restore_from_has`].
    ///
    /// Loads all levels concurrently via `std::thread::scope`. Requires `Fn + Send + Sync`
    /// instead of `FnMut` so the closure can be shared across threads. On a machine with
    /// fast disk I/O this reduces restore time from ~156s to ~56s (bounded by level 10).
    ///
    /// # Arguments
    ///
    /// * `hashes` - Vec of (curr_hash, snap_hash) pairs for each level
    /// * `next_states` - Vec of HasNextState for each level
    /// * `load_bucket` - Thread-safe function to load a bucket from its hash
    pub fn restore_from_has_parallel<F>(
        hashes: &[(Hash256, Hash256)],
        next_states: &[HasNextState],
        load_bucket: F,
    ) -> Result<Self>
    where
        F: Fn(&Hash256) -> Result<Bucket> + Send + Sync,
    {
        if hashes.len() != BUCKET_LIST_LEVELS {
            return Err(BucketError::Serialization(format!(
                "Expected {} bucket level hashes, got {}",
                BUCKET_LIST_LEVELS,
                hashes.len()
            )));
        }

        let load_bucket = &load_bucket;

        // Collect (level_index, output_hash) for levels with completed merge outputs.
        let output_hashes: Vec<(usize, Hash256)> = next_states
            .iter()
            .enumerate()
            .filter_map(|(i, state)| {
                if state.state == HAS_NEXT_STATE_OUTPUT {
                    state
                        .output
                        .as_ref()
                        .filter(|h| !h.is_zero())
                        .map(|h| (i, *h))
                } else {
                    None
                }
            })
            .collect();

        let levels = std::thread::scope(|s| -> Result<Vec<BucketLevel>> {
            // Spawn one thread per level to load curr + snap.
            let level_handles: Vec<_> = hashes
                .iter()
                .zip(next_states.iter())
                .enumerate()
                .map(|(i, ((curr_hash, snap_hash), _state))| {
                    s.spawn(move || -> Result<(usize, Bucket, Bucket)> {
                        let level_start = std::time::Instant::now();

                        let curr = if curr_hash.is_zero() {
                            Bucket::empty()
                        } else {
                            load_bucket(curr_hash)?
                        };

                        let snap = if snap_hash.is_zero() {
                            Bucket::empty()
                        } else {
                            load_bucket(snap_hash)?
                        };

                        tracing::info!(
                            level = i,
                            curr_entries = curr.len(),
                            snap_entries = snap.len(),
                            elapsed_ms = level_start.elapsed().as_millis() as u64,
                            "restore_from_has_parallel: loaded level curr+snap"
                        );

                        Ok((i, curr, snap))
                    })
                })
                .collect();

            // Spawn separate threads for output buckets (completed merge outputs).
            // These run fully in parallel with the level curr/snap threads so that
            // a large output bucket (e.g., level 10's ~47M-entry merge result) does
            // not extend the critical path beyond the largest curr/snap load.
            let output_handles: Vec<(usize, _)> = output_hashes
                .iter()
                .map(|(level_idx, hash)| {
                    let i = *level_idx;
                    let hash = *hash;
                    (
                        i,
                        s.spawn(move || -> Result<Bucket> {
                            let t = std::time::Instant::now();
                            let bucket = load_bucket(&hash)?;
                            tracing::info!(
                                level = i,
                                entries = bucket.len(),
                                elapsed_ms = t.elapsed().as_millis() as u64,
                                "restore_from_has_parallel: loaded output bucket"
                            );
                            Ok(bucket)
                        }),
                    )
                })
                .collect();

            // Collect level curr/snap results (order matches input since indexed by i).
            let level_results: Vec<(usize, Bucket, Bucket)> = level_handles
                .into_iter()
                .map(|h| h.join().expect("level load thread panicked"))
                .collect::<Result<_>>()?;

            // Collect output bucket results into a map keyed by level index.
            let mut output_map: std::collections::HashMap<usize, Bucket> = output_handles
                .into_iter()
                .map(|(level_idx, h)| {
                    h.join()
                        .expect("output bucket thread panicked")
                        .map(|b| (level_idx, b))
                })
                .collect::<Result<_>>()?;

            // Assemble BucketLevels, attaching output buckets where present.
            // level_results is already in level order (spawned from enumerate).
            let mut levels: Vec<BucketLevel> = Vec::with_capacity(BUCKET_LIST_LEVELS);
            for (i, curr, snap) in level_results {
                let next = output_map.remove(&i).map(PendingMerge::InMemory);
                let mut level = BucketLevel::new(i);
                level.curr = Arc::new(curr);
                level.snap = Arc::new(snap);
                level.next = next;
                levels.push(level);
            }
            Ok(levels)
        })?;

        Ok(Self {
            levels,
            ledger_seq: 0,
            bucket_dir: None,
            bucket_list_db_config: None,
            completed_merges: Vec::new(),
            merge_map: None,
            merge_counters: Arc::new(MergeCounters::new()),
            pending_persist: None,
        })
    }

    /// Restore a bucket list from History Archive State with full FutureBucket support.
    ///
    /// This is the primary restoration method. It restores pending merge results when
    /// the HAS indicates a completed merge (state == HAS_NEXT_STATE_OUTPUT), which is
    /// necessary for correct bucket list hash computation at checkpoints.
    ///
    /// For convenience, [`Self::restore_from_hashes`] wraps this function with default
    /// next states (all state=0).
    ///
    /// # Arguments
    ///
    /// * `hashes` - Vec of (curr_hash, snap_hash) pairs for each level
    /// * `next_states` - Vec of HasNextState for each level
    /// * `load_bucket` - Function to load a bucket from its hash
    pub fn restore_from_has<F>(
        hashes: &[(Hash256, Hash256)],
        next_states: &[HasNextState],
        mut load_bucket: F,
    ) -> Result<Self>
    where
        F: FnMut(&Hash256) -> Result<Bucket>,
    {
        if hashes.len() != BUCKET_LIST_LEVELS {
            return Err(BucketError::Serialization(format!(
                "Expected {} bucket level hashes, got {}",
                BUCKET_LIST_LEVELS,
                hashes.len()
            )));
        }

        let mut levels = Vec::with_capacity(BUCKET_LIST_LEVELS);

        for (i, (curr_hash, snap_hash)) in hashes.iter().enumerate() {
            let level_start = std::time::Instant::now();
            let curr = if curr_hash.is_zero() {
                Bucket::empty()
            } else {
                load_bucket(curr_hash)?
            };

            let snap = if snap_hash.is_zero() {
                Bucket::empty()
            } else {
                load_bucket(snap_hash)?
            };

            tracing::info!(
                level = i,
                curr_entries = curr.len(),
                snap_entries = snap.len(),
                elapsed_ms = level_start.elapsed().as_millis() as u64,
                "restore_from_has: loaded level"
            );

            // Check if there's a completed merge (state == HAS_NEXT_STATE_OUTPUT) for this level
            let next: Option<PendingMerge> = if let Some(state) = next_states.get(i) {
                if state.state == HAS_NEXT_STATE_OUTPUT {
                    if let Some(ref output_hash) = state.output {
                        if !output_hash.is_zero() {
                            tracing::debug!(
                                level = i,
                                output_hash = %output_hash.to_hex(),
                                "restore_from_has: loading completed merge output"
                            );
                            Some(PendingMerge::InMemory(load_bucket(output_hash)?))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    // For state 2 (HAS_NEXT_STATE_INPUTS), we don't set next here.
                    // The merge will be restarted in restart_merges_from_has.
                    None
                }
            } else {
                None
            };

            let mut level = BucketLevel::new(i);
            level.curr = Arc::new(curr);
            level.snap = Arc::new(snap);
            level.next = next;
            levels.push(level);
        }

        Ok(Self {
            levels,
            ledger_seq: 0,
            bucket_dir: None,
            bucket_list_db_config: None,
            completed_merges: Vec::new(),
            merge_map: None,
            merge_counters: Arc::new(MergeCounters::new()),
            pending_persist: None,
        })
    }

    /// Restart any pending merges after restoring from a History Archive State (HAS),
    /// using the stored input hashes from the HAS.
    ///
    /// This handles state 2 (HAS_NEXT_STATE_INPUTS) by restarting merges with the
    /// exact input curr and snap hashes stored in the HAS. All level merges run
    /// concurrently via `tokio::task::spawn_blocking`, reducing merge restart time
    /// from ~4 minutes (sequential) to ~92 seconds (limited by the slowest level).
    ///
    /// After parallel merges complete, falls through to `restart_merges` for
    /// structure-based fallback (levels with state 0) if `restart_structure_based`
    /// is true.
    ///
    /// # Arguments
    ///
    /// * `restart_structure_based` - If true, fall back to structure-based merge restart
    ///   for levels with no stored HAS hashes (state 0). This should be true for full
    ///   startup mode but false for offline verification mode. stellar-core only
    ///   calls restartMerges in full startup mode, not for standalone offline commands.
    ///
    /// # Panics
    ///
    /// Panics if called outside of a tokio runtime context.
    pub async fn restart_merges_from_has<F>(
        &mut self,
        ledger: u32,
        protocol_version: u32,
        next_states: &[HasNextState],
        mut load_bucket: F,
        restart_structure_based: bool,
    ) -> Result<()>
    where
        F: FnMut(&Hash256) -> Result<Bucket>,
    {
        tracing::debug!(
            ledger = ledger,
            "restart_merges_from_has: restarting merges using HAS input hashes (parallel)"
        );

        // Phase 1: Collect work items (sequential, fast — just loads input buckets)
        struct MergeWorkItem {
            level: usize,
            input_curr: Bucket,
            input_snap: Bucket,
            keep_dead: bool,
        }

        let mut work_items = Vec::new();

        for i in 1..BUCKET_LIST_LEVELS {
            // Skip if there's already a pending merge (from state 1 output)
            if self.levels[i].next.is_some() {
                tracing::trace!(
                    level = i,
                    "restart_merges_from_has: level already has pending merge"
                );
                continue;
            }

            if let Some(state) = next_states.get(i) {
                if state.state == HAS_NEXT_STATE_INPUTS {
                    if let (Some(ref curr_hash), Some(ref snap_hash)) =
                        (&state.input_curr, &state.input_snap)
                    {
                        let input_curr = if curr_hash.is_zero() {
                            Bucket::empty()
                        } else {
                            load_bucket(curr_hash)?
                        };

                        let input_snap = if snap_hash.is_zero() {
                            Bucket::empty()
                        } else {
                            load_bucket(snap_hash)?
                        };

                        tracing::info!(
                            level = i,
                            ledger = ledger,
                            input_curr_hash = %curr_hash.to_hex(),
                            input_snap_hash = %snap_hash.to_hex(),
                            "restart_merges_from_has: queueing merge"
                        );

                        work_items.push(MergeWorkItem {
                            level: i,
                            input_curr,
                            input_snap,
                            keep_dead: Self::keep_tombstone_entries(i),
                        });
                    }
                }
            }
        }

        // Phase 2: Spawn all merges in parallel via spawn_blocking
        if !work_items.is_empty() {
            let bucket_dir = self.bucket_dir.clone();

            let handles: Vec<_> = work_items
                .into_iter()
                .map(|work| {
                    let bucket_dir = bucket_dir.clone();
                    tokio::task::spawn_blocking(move || {
                        let start = std::time::Instant::now();
                        let level = work.level;

                        let result = perform_merge(
                            &work.input_curr,
                            &work.input_snap,
                            bucket_dir.as_ref(),
                            work.keep_dead,
                            protocol_version,
                        );

                        let elapsed = start.elapsed();
                        match &result {
                            Ok(bucket) => {
                                tracing::info!(
                                    level,
                                    duration_ms = elapsed.as_millis() as u64,
                                    merged_hash = %bucket.hash().to_hex(),
                                    "restart_merges_from_has: merge completed"
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    level,
                                    duration_ms = elapsed.as_millis() as u64,
                                    error = %e,
                                    "restart_merges_from_has: merge failed"
                                );
                            }
                        }

                        result.map(|bucket| (level, bucket))
                    })
                })
                .collect();

            // Phase 3: Await all and install results
            let results = futures::future::join_all(handles).await;
            for join_result in results {
                let (level, merged) = join_result
                    .map_err(|e| BucketError::Merge(format!("merge task panicked: {}", e)))??;
                self.levels[level].next = Some(PendingMerge::InMemory(merged));
            }
        }

        // For levels that don't have HAS input hashes (state 0 = clear),
        // fall back to regular restart_merges which examines bucket structure
        // to determine if a merge should be in progress.
        //
        // This matches stellar-core behavior: when next.isClear() for a level,
        // restartMerges() uses the previous level's snap to start a merge if needed.
        //
        // Note: stellar-core only does structure-based restarts in full startup mode, not for
        // standalone offline commands. The caller controls this via restart_structure_based.
        if restart_structure_based {
            self.restart_merges(ledger, protocol_version)
        } else {
            self.ledger_seq = ledger;
            Ok(())
        }
    }

    /// Restart any pending merges after restoring from a History Archive State (HAS).
    ///
    /// When a bucket list is restored from HAS, there may be merges that should have been
    /// in progress at that checkpoint ledger. This function recreates those pending merges
    /// by examining the current and snap buckets and starting merges where appropriate.
    ///
    /// This matches stellar-core's BucketListBase::restartMerges().
    ///
    /// For each level > 0 with no pending merge:
    /// 1. Check if the previous level's snap is non-empty
    /// 2. If so, start a merge using that snap
    /// 3. The merge will be committed when the next spill occurs
    pub fn restart_merges(&mut self, ledger: u32, protocol_version: u32) -> Result<()> {
        tracing::debug!(
            ledger = ledger,
            "restart_merges: restarting pending merges after HAS restore"
        );

        for i in 1..BUCKET_LIST_LEVELS {
            // Skip if there's already a pending merge
            if self.levels[i].next.is_some() {
                tracing::trace!(level = i, "restart_merges: level already has pending merge");
                continue;
            }

            // Clone the previous level's snap to avoid borrow conflicts
            let prev_snap = self.levels[i - 1].snap.clone();

            // If the previous level's snap is empty, this and all higher levels
            // are uninitialized (haven't received enough data yet)
            if prev_snap.is_empty() {
                tracing::debug!(
                    level = i,
                    "restart_merges: previous level snap is empty, stopping"
                );
                break;
            }

            // Calculate the ledger when this merge would have started
            // This is roundDown(ledger, levelHalf(i - 1))
            let merge_start_ledger = bl_round_down(ledger, bl_level_half(i - 1));

            tracing::debug!(
                level = i,
                merge_start_ledger = merge_start_ledger,
                prev_snap_hash = %prev_snap.hash(),
                "restart_merges: restarting merge"
            );

            // Determine merge parameters
            let merge_protocol_version = prev_snap.protocol_version().unwrap_or(protocol_version);
            // Note: stellar-core never normalizes INIT to LIVE during merges - the keepTombstoneEntries
            // flag only affects DEAD entry filtering, not INIT entry transformation.
            let keep_dead = Self::keep_tombstone_entries(i);
            let normalize_init = false; // stellar-core never normalizes INIT to LIVE during merges
            let use_empty_curr = Self::should_merge_with_empty_curr(merge_start_ledger, i);

            // Log detailed merge parameters for debugging
            tracing::info!(
                level = i,
                ledger = ledger,
                merge_start_ledger = merge_start_ledger,
                use_empty_curr = use_empty_curr,
                level_curr_hash = %self.levels[i].curr.hash().to_hex(),
                level_snap_hash = %self.levels[i].snap.hash().to_hex(),
                prev_snap_hash = %prev_snap.hash().to_hex(),
                "restart_merges: starting merge"
            );

            // Start the merge with the previous level's snap
            self.levels[i].prepare_with_normalization(
                merge_protocol_version,
                prev_snap,
                &[],
                MergeContext {
                    keep_dead_entries: keep_dead,
                    normalize_init,
                    use_empty_curr,
                    bucket_dir: self.bucket_dir.as_deref(),
                    merge_map: self.merge_map.as_ref(),
                    merge_counters: Some(Arc::clone(&self.merge_counters)),
                },
            )?;

            tracing::info!(
                level = i,
                next_hash = %self.levels[i].next.as_ref().map(|b| b.hash().to_hex()).unwrap_or_else(|| "None".to_string()),
                "restart_merges: merge restarted successfully"
            );
        }

        // Update the ledger sequence to the restored ledger
        self.ledger_seq = ledger;

        Ok(())
    }

    /// Get statistics about the bucket list.
    pub fn stats(&self) -> BucketListStats {
        let mut total_entries = 0;
        let mut total_buckets = 0;

        for level in &self.levels {
            if !level.curr.is_empty() {
                total_entries += level.curr.len();
                total_buckets += 1;
            }
            if !level.snap.is_empty() {
                total_entries += level.snap.len();
                total_buckets += 1;
            }
        }

        BucketListStats {
            num_levels: BUCKET_LIST_LEVELS,
            total_entries,
            total_buckets,
        }
    }

    /// Debug print level state.
    pub fn debug_print_levels(&self) {
        for (i, level) in self.levels.iter().enumerate() {
            let curr_hash = level.curr.hash();
            let snap_hash = level.snap.hash();
            let next_hash = level
                .next
                .as_ref()
                .map(|b| b.hash().to_hex())
                .unwrap_or_else(|| "None".to_string());
            eprintln!(
                "  L{}: curr={}, snap={}, next={}",
                i,
                curr_hash.to_hex(),
                snap_hash.to_hex(),
                next_hash
            );
        }
    }

    /// Scan for expired Soroban entries in the bucket list.
    ///
    /// This scans all Soroban entries (ContractData, ContractCode) and checks their
    /// TTL entries to determine which have expired.
    ///
    /// Returns:
    /// - `archived_entries`: Persistent entries (ContractCode or persistent ContractData)
    ///   that should be archived to the hot archive bucket list
    /// - `deleted_keys`: Temporary entries that should be deleted
    ///
    /// An entry is considered expired when its TTL's `live_until_ledger_seq < current_ledger`.
    /// Scan the entire bucket list for expired entries (non-incremental).
    ///
    /// Iterates levels from shallowest (newest) to deepest (oldest). Because
    /// shallowest entries shadow deeper ones, the first occurrence of each key
    /// is authoritative and a separate point-lookup is unnecessary — the
    /// iteration order itself provides deduplication.
    ///
    /// BUCKETLISTDB_SPEC §8.3: Temporary entries are deleted; persistent entries
    /// are archived to the hot archive bucket list.
    pub fn scan_for_eviction(
        &self,
        current_ledger: u32,
    ) -> Result<(Vec<LedgerEntry>, Vec<LedgerKey>)> {
        let mut archived_entries: Vec<LedgerEntry> = Vec::new();
        let mut deleted_keys: Vec<LedgerKey> = Vec::new();

        // Track which keys we've already processed (to avoid duplicates from different levels).
        // Shallowest-first iteration means the first occurrence is the newest version.
        let mut seen_keys: HashSet<Vec<u8>> = HashSet::new();

        // Iterate through all levels from shallowest (newest) to deepest (oldest)
        for level in &self.levels {
            for bucket in [&level.curr, &level.snap] {
                for entry_result in bucket.iter()? {
                    let entry = entry_result?;
                    // Only process LIVE and INIT entries (not DEAD or Metadata)
                    let live_entry = match entry {
                        BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) => e,
                        BucketEntry::Deadentry(key) => {
                            // Mark dead keys as seen so we don't process older versions
                            let key_bytes = key.to_xdr(Limits::none()).map_err(|e| {
                                BucketError::Serialization(format!(
                                    "failed to serialize ledger key: {}",
                                    e
                                ))
                            })?;
                            seen_keys.insert(key_bytes);
                            continue;
                        }
                        BucketEntry::Metaentry(_) => continue,
                    };

                    // Only check Soroban entries (ContractData, ContractCode)
                    if !is_soroban_entry(&live_entry) {
                        continue;
                    }

                    // Get the key for this entry
                    let key = henyey_common::entry_to_key(&live_entry);

                    // Check if we've already processed this key
                    let key_bytes = key.to_xdr(Limits::none()).map_err(|e| {
                        BucketError::Serialization(format!("failed to serialize ledger key: {}", e))
                    })?;
                    if !seen_keys.insert(key_bytes) {
                        // Already processed this key from a newer level
                        continue;
                    }

                    // Get the TTL key for this Soroban entry
                    let Some(ttl_key) = get_ttl_key(&key) else {
                        continue;
                    };

                    // Look up the TTL entry in the bucket list
                    let Some(ttl_entry) = self.get(&ttl_key)? else {
                        // No TTL entry found - this shouldn't happen for valid Soroban entries
                        // but we skip rather than error
                        tracing::warn!(?key, "Soroban entry has no TTL entry during eviction scan");
                        continue;
                    };

                    // Check if the entry is expired
                    let Some(is_expired) = is_ttl_expired(&ttl_entry, current_ledger) else {
                        // Not a TTL entry (shouldn't happen)
                        continue;
                    };

                    if !is_expired {
                        // Entry is still live, skip it
                        continue;
                    }

                    // Entry is expired - categorize it
                    if is_temporary_entry(&live_entry) {
                        // Temporary entries are deleted
                        deleted_keys.push(key);
                    } else if is_persistent_entry(&live_entry) {
                        // Persistent entries are archived to hot archive
                        archived_entries.push(live_entry);
                    }
                }
            }
        }

        tracing::debug!(
            current_ledger,
            archived_count = archived_entries.len(),
            deleted_count = deleted_keys.len(),
            "Eviction scan completed"
        );

        Ok((archived_entries, deleted_keys))
    }

    /// Perform an incremental eviction scan starting from the given iterator position.
    ///
    /// This matches stellar-core's `scanForEviction` behavior:
    /// - Scans entries starting from the iterator's current position
    /// - Stops when `settings.eviction_scan_size` bytes have been scanned
    /// - Updates the iterator to the new position
    /// - Returns evicted entries (archived persistent + deleted temporary)
    ///
    /// The scan automatically advances through buckets when reaching the end of one.
    pub fn scan_for_eviction_incremental(
        &self,
        mut iter: EvictionIterator,
        current_ledger: u32,
        settings: &StateArchivalSettings,
    ) -> Result<EvictionResult> {
        let mut result = EvictionResult {
            candidates: Vec::new(),
            end_iterator: iter.clone(),
            bytes_scanned: 0,
            scan_complete: false,
        };

        // Update iterator based on spills (reset offset if bucket received new data)
        update_starting_eviction_iterator(
            &mut iter,
            settings.starting_eviction_scan_level,
            current_ledger,
        );

        let start_iter = iter.clone();
        let mut bytes_remaining = settings.eviction_scan_size as u64;

        // Track keys we've seen to avoid duplicates (from shadowed entries)
        let mut seen_keys: HashSet<Vec<u8>> = HashSet::new();

        loop {
            // Get the current bucket
            let level = iter.bucket_list_level as usize;
            if level >= BUCKET_LIST_LEVELS {
                // Wrapped around, done
                result.scan_complete = true;
                break;
            }

            let bucket = if iter.is_curr_bucket {
                &self.levels[level].curr
            } else {
                &self.levels[level].snap
            };

            // Scan entries in this bucket (byte-limited only, no entry count limit)
            let (_entries_scanned, bytes_used, finished_bucket) = self.scan_bucket_region(
                bucket,
                &mut iter,
                bytes_remaining,
                current_ledger,
                &mut result.candidates,
                &mut seen_keys,
            )?;

            result.bytes_scanned += bytes_used;

            if bytes_remaining > bytes_used {
                bytes_remaining -= bytes_used;
            } else {
                bytes_remaining = 0;
            }

            // If we've hit the byte limit, we're done
            if bytes_remaining == 0 {
                result.scan_complete = true;
                break;
            }

            // If we finished this bucket, move to the next one
            if finished_bucket {
                iter.advance_to_next_bucket(settings.starting_eviction_scan_level);

                // Check if we've completed a full cycle - only break when we return
                // to the exact starting bucket (same level AND same is_curr).
                if iter.bucket_list_level == start_iter.bucket_list_level
                    && iter.is_curr_bucket == start_iter.is_curr_bucket
                {
                    result.scan_complete = true;
                    break;
                }
            }
        }

        result.end_iterator = iter;

        Ok(result)
    }

    /// Scan a region of a bucket for evictable entries (scan phase only).
    ///
    /// Returns (entries_scanned, bytes_used, finished_bucket).
    ///
    /// This is the scan phase of the two-phase eviction approach. It collects
    /// ALL eligible candidates within the byte budget. The `max_entries_to_archive`
    /// limit is NOT applied here — it's applied in the resolution phase via
    /// `EvictionResult::resolve()`.
    ///
    /// Uses byte-offset-aware iteration: for disk-backed buckets, seeks directly
    /// to the start offset (instead of reading and skipping millions of entries)
    /// and reads record sizes from the file format (instead of re-serializing
    /// every entry to XDR just for byte size computation).
    fn scan_bucket_region(
        &self,
        bucket: &Bucket,
        iter: &mut EvictionIterator,
        max_bytes: u64,
        current_ledger: u32,
        candidates: &mut Vec<EvictionCandidate>,
        seen_keys: &mut HashSet<Vec<u8>>,
    ) -> Result<(usize, u64, bool)> {
        let mut entries_scanned = 0;
        let mut bytes_used = 0u64;

        // Skip buckets that predate Soroban; they cannot contain evictable entries.
        let bucket_protocol = bucket.protocol_version().unwrap_or(0);
        if bucket_protocol < MIN_SOROBAN_PROTOCOL_VERSION {
            iter.bucket_file_offset = 0;
            return Ok((entries_scanned, bytes_used, true));
        }

        // bucket_file_offset is a byte offset in the bucket file.
        let start_offset = iter.bucket_file_offset;

        // Use offset-aware iteration: for disk-backed buckets this seeks directly
        // to start_offset and reads record sizes from record marks (no XDR
        // re-serialization). For in-memory buckets it computes sizes on the fly
        // (acceptable since in-memory buckets are small).
        for result in bucket.iter_from_offset_with_sizes(start_offset)? {
            let (entry, entry_size) = result?;
            bytes_used += entry_size;
            entries_scanned += 1;

            // Process the entry for eviction. The labeled block lets each
            // skip-branch `break 'process` so that the single budget check
            // at the end of the loop body is always reached.
            'process: {
                let live_entry = match &entry {
                    BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) => e,
                    BucketEntry::Deadentry(key) => {
                        // Mark dead keys as seen
                        if let Ok(key_bytes) = key.to_xdr(Limits::none()) {
                            seen_keys.insert(key_bytes);
                        }
                        break 'process;
                    }
                    BucketEntry::Metaentry(_) => {
                        break 'process;
                    }
                };

                // Only check Soroban entries
                if !is_soroban_entry(live_entry) {
                    break 'process;
                }

                // Get the key for this entry
                let key = henyey_common::entry_to_key(live_entry);

                // Check if we've already seen this key (from a newer bucket)
                let key_bytes = match key.to_xdr(Limits::none()) {
                    Ok(bytes) => bytes,
                    Err(_) => break 'process,
                };

                if !seen_keys.insert(key_bytes) {
                    break 'process;
                }

                // Get the TTL key
                let Some(ttl_key) = get_ttl_key(&key) else {
                    break 'process;
                };

                // Look up the TTL entry
                let Some(ttl_entry) = self.get(&ttl_key)? else {
                    break 'process;
                };

                // Check if expired
                let Some(is_expired) = is_ttl_expired(&ttl_entry, current_ledger) else {
                    break 'process;
                };

                if !is_expired {
                    break 'process;
                }

                // Entry is expired — collect as eviction candidate.
                // For persistent entries, archive the NEWEST version from the bucket list
                // (not the potentially stale version from the older bucket being scanned).
                // See: BucketSnapshot.cpp scanForEviction() lines 247-261
                let is_temp = is_temporary_entry(live_entry);
                let entry_for_candidate = if !is_temp {
                    if let Some(newest_entry) = self.get(&key)? {
                        newest_entry
                    } else {
                        live_entry.clone()
                    }
                } else {
                    live_entry.clone()
                };

                candidates.push(EvictionCandidate {
                    entry: entry_for_candidate,
                    data_key: key,
                    ttl_key,
                    is_temporary: is_temp,
                    position: EvictionIterator {
                        bucket_list_level: iter.bucket_list_level,
                        is_curr_bucket: iter.is_curr_bucket,
                        bucket_file_offset: start_offset + bytes_used,
                    },
                });
            }

            // Single budget check: stop scanning if we've consumed enough bytes.
            // (max_entries limit is applied in the resolution phase, not here.)
            if bytes_used >= max_bytes {
                break;
            }
        }

        // Update the iterator position. The third element indicates whether
        // the bucket was fully scanned (`true`) or we stopped early because
        // the byte budget was exhausted (`false`).
        let budget_exhausted = bytes_used >= max_bytes;
        iter.bucket_file_offset = start_offset + bytes_used;
        Ok((entries_scanned, bytes_used, !budget_exhausted))
    }
}

impl Default for BucketList {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BucketList {
    fn drop(&mut self) {
        // Wait for any pending background persist to complete
        if let Some(handle) = self.pending_persist.take() {
            handle.join().ok();
        }
    }
}

impl Clone for BucketList {
    fn clone(&self) -> Self {
        Self {
            levels: self.levels.clone(),
            ledger_seq: self.ledger_seq,
            bucket_dir: self.bucket_dir.clone(),
            bucket_list_db_config: self.bucket_list_db_config.clone(),
            completed_merges: self.completed_merges.clone(),
            merge_map: self.merge_map.clone(),
            merge_counters: self.merge_counters.clone(),
            pending_persist: None, // Background persist is not cloned
        }
    }
}

impl std::fmt::Debug for BucketList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BucketList")
            .field("ledger_seq", &self.ledger_seq)
            .field("hash", &self.hash().to_hex())
            .field("stats", &self.stats())
            .finish()
    }
}

/// Statistics about a BucketList.
///
/// Provides summary information about the bucket list state, useful for
/// monitoring, debugging, and capacity planning.
#[derive(Debug, Clone)]
pub struct BucketListStats {
    /// Number of levels in the bucket list (always 11).
    pub num_levels: usize,
    /// Total number of entries across all buckets (including metadata).
    pub total_entries: usize,
    /// Total number of non-empty buckets (max 22: 11 levels * 2 buckets each).
    pub total_buckets: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::BucketEntry as BucketListEntry;
    use crate::merge::merge_buckets_with_options;
    use stellar_xdr::curr::*;

    const TEST_PROTOCOL: u32 = 25;

    fn make_account_id(bytes: [u8; 32]) -> AccountId {
        AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
    }

    fn make_account_entry(bytes: [u8; 32], balance: i64) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: make_account_id(bytes),
                balance,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: Vec::new().try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_account_key(bytes: [u8; 32]) -> LedgerKey {
        LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id(bytes),
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_new_bucket_list() {
        let bl = BucketList::new();
        assert_eq!(bl.levels().len(), BUCKET_LIST_LEVELS);
        assert_eq!(bl.ledger_seq(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_add_batch_simple() {
        let mut bl = BucketList::new();

        let entry = make_account_entry([1u8; 32], 100);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![entry],
            vec![],
            vec![],
        )
        .unwrap();

        let key = make_account_key([1u8; 32]);
        let found = bl.get(&key).unwrap().unwrap();

        if let LedgerEntryData::Account(account) = &found.data {
            assert_eq!(account.balance, 100);
        } else {
            panic!("Expected Account entry");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_add_batch_update() {
        let mut bl = BucketList::new();

        // Add initial entry
        let entry1 = make_account_entry([1u8; 32], 100);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![entry1],
            vec![],
            vec![],
        )
        .unwrap();

        // Update entry
        let entry2 = make_account_entry([1u8; 32], 200);
        bl.add_batch(
            2,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![entry2],
            vec![],
        )
        .unwrap();

        let key = make_account_key([1u8; 32]);
        let found = bl.get(&key).unwrap().unwrap();

        if let LedgerEntryData::Account(account) = &found.data {
            assert_eq!(account.balance, 200);
        } else {
            panic!("Expected Account entry");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_live_entries_respects_deletes() {
        let mut bl = BucketList::new();

        let entry = make_account_entry([1u8; 32], 100);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![entry],
            vec![],
            vec![],
        )
        .unwrap();

        let dead = make_account_key([1u8; 32]);
        bl.add_batch(
            2,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![],
            vec![dead],
        )
        .unwrap();

        let entries: Vec<_> = bl.live_entries_iter().collect::<Result<Vec<_>>>().unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_add_batch_delete() {
        let mut bl = BucketList::new();

        // Add entry
        let entry = make_account_entry([1u8; 32], 100);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![entry],
            vec![],
            vec![],
        )
        .unwrap();

        // Delete entry
        let key = make_account_key([1u8; 32]);
        bl.add_batch(
            2,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![],
            vec![key.clone()],
        )
        .unwrap();

        // Should not be found
        let found = bl.get(&key).unwrap();
        assert!(found.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_level_sizes() {
        assert_eq!(bl_level_size(0), 4);
        assert_eq!(bl_level_size(1), 16);
        assert_eq!(bl_level_size(2), 64);
        assert_eq!(bl_level_size(3), 256);
        assert_eq!(bl_level_half(0), 2);
        assert_eq!(bl_level_half(1), 8);
        assert_eq!(bl_level_half(2), 32);
        assert_eq!(bl_level_half(3), 128);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_level_should_spill_boundaries() {
        // Level 0 spills at even ledgers (multiples of 2)
        assert!(BucketList::level_should_spill(0, 0));
        assert!(BucketList::level_should_spill(2, 0));
        assert!(BucketList::level_should_spill(4, 0));
        assert!(!BucketList::level_should_spill(1, 0));
        assert!(!BucketList::level_should_spill(3, 0));

        // Level 1 spills at multiples of 8
        assert!(BucketList::level_should_spill(0, 1));
        assert!(BucketList::level_should_spill(8, 1));
        assert!(BucketList::level_should_spill(16, 1));
        assert!(!BucketList::level_should_spill(4, 1));
        assert!(!BucketList::level_should_spill(12, 1));

        // Level 2 spills at multiples of 32
        assert!(BucketList::level_should_spill(0, 2));
        assert!(BucketList::level_should_spill(32, 2));
        assert!(BucketList::level_should_spill(64, 2));
        assert!(!BucketList::level_should_spill(16, 2));

        // Top level never spills
        assert!(!BucketList::level_should_spill(0, BUCKET_LIST_LEVELS - 1));
        assert!(!BucketList::level_should_spill(64, BUCKET_LIST_LEVELS - 1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_prepare_with_normalization_converts_init() {
        let mut level = BucketLevel::new(BUCKET_LIST_LEVELS - 1);
        let entry = make_account_entry([1u8; 32], 100);
        let meta = BucketMetadata {
            ledger_version: TEST_PROTOCOL,
            ext: BucketMetadataExt::V1(BucketListType::Live),
        };
        let incoming = Bucket::from_entries(vec![
            BucketListEntry::Metaentry(meta),
            BucketListEntry::Initentry(entry.clone()),
        ])
        .unwrap();

        level
            .prepare_with_normalization(
                TEST_PROTOCOL,
                Arc::new(incoming),
                &[],
                MergeContext {
                    keep_dead_entries: false,
                    normalize_init: true,
                    use_empty_curr: false,
                    bucket_dir: None,
                    merge_map: None,
                    merge_counters: None,
                },
            )
            .unwrap();
        let _ = level.commit().unwrap();

        let mut saw_live = false;
        for entry_result in level.curr.iter().unwrap() {
            let entry = entry_result.unwrap();
            match entry {
                BucketListEntry::Liveentry(live) => {
                    saw_live = true;
                    assert!(matches!(live.data, LedgerEntryData::Account(_)));
                }
                BucketListEntry::Initentry(_) => panic!("init entry should be normalized"),
                _ => {}
            }
        }
        assert!(saw_live);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_merge_drops_dead_when_keep_dead_false() {
        let key = make_account_key([1u8; 32]);
        let bucket = Bucket::from_entries(vec![BucketListEntry::Deadentry(key)]).unwrap();
        let merged =
            merge_buckets_with_options(&Bucket::empty(), &bucket, false, TEST_PROTOCOL, true)
                .unwrap();
        let mut has_non_meta = false;
        for entry_result in merged.iter().unwrap() {
            let entry = entry_result.unwrap();
            if !entry.is_metadata() {
                has_non_meta = true;
                break;
            }
        }
        assert!(!has_non_meta);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bucket_list_hash_changes() {
        let mut bl = BucketList::new();
        let hash1 = bl.hash();

        let entry = make_account_entry([1u8; 32], 100);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![entry],
            vec![],
            vec![],
        )
        .unwrap();
        let hash2 = bl.hash();

        assert_ne!(hash1, hash2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_contains() {
        let mut bl = BucketList::new();

        let key = make_account_key([1u8; 32]);
        assert!(!bl.contains(&key).unwrap());

        let entry = make_account_entry([1u8; 32], 100);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![entry],
            vec![],
            vec![],
        )
        .unwrap();

        assert!(bl.contains(&key).unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_multiple_levels() {
        let mut bl = BucketList::new();

        // Add many entries to trigger spills to higher levels
        for i in 1..=20u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            let entry = make_account_entry(id, i as i64 * 100);
            bl.add_batch(
                i,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![entry],
                vec![],
                vec![],
            )
            .unwrap();
        }

        // Verify all entries are accessible
        for i in 1..=20u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            let key = make_account_key(id);
            let found = bl.get(&key).unwrap();
            assert!(found.is_some(), "Entry {} not found", i);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_should_merge_with_empty_curr() {
        // Level 0 always returns false
        assert!(!BucketList::should_merge_with_empty_curr(1, 0));
        assert!(!BucketList::should_merge_with_empty_curr(2, 0));
        assert!(!BucketList::should_merge_with_empty_curr(100, 0));

        // Level 1: half=8, size=16
        // At ledger 2: mergeStartLedger=2, nextChangeLedger=4
        // levelShouldSpill(4, 1) = false (4 is not at 8 or 16 boundary)
        assert!(!BucketList::should_merge_with_empty_curr(2, 1));

        // At ledger 4: mergeStartLedger=4, nextChangeLedger=6
        // levelShouldSpill(6, 1) = false
        assert!(!BucketList::should_merge_with_empty_curr(4, 1));

        // At ledger 6: mergeStartLedger=6, nextChangeLedger=8
        // levelShouldSpill(8, 1) = true (8 is at half boundary for level 1)
        assert!(BucketList::should_merge_with_empty_curr(6, 1));

        // At ledger 8: mergeStartLedger=8, nextChangeLedger=10
        // levelShouldSpill(10, 1) = false
        assert!(!BucketList::should_merge_with_empty_curr(8, 1));

        // At ledger 14: mergeStartLedger=14, nextChangeLedger=16
        // levelShouldSpill(16, 1) = true (16 is at size boundary for level 1)
        assert!(BucketList::should_merge_with_empty_curr(14, 1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_entries_preserved_across_should_merge_with_empty_curr() {
        // This test specifically verifies that entries are not lost when
        // shouldMergeWithEmptyCurr returns true.
        //
        // The critical ledger sequence for level 1 is:
        // - Ledger 4: Entry added, spill to level 1, shouldMergeWithEmptyCurr(4,1)=false
        // - Ledger 6: Entry added, spill to level 1, shouldMergeWithEmptyCurr(6,1)=true
        // - Ledger 8: Entry added, level 1 snaps, shouldMergeWithEmptyCurr(8,1)=false
        //
        // Entry from ledger 4 should be accessible at ledger 6 (in level 1's next),
        // at ledger 7 (in level 1's next), and at ledger 8 (in level 1's snap).

        let mut bl = BucketList::new();

        // Add entries at ledgers 1-8
        for ledger in 1..=8u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&ledger.to_be_bytes());
            let entry = make_account_entry(id, ledger as i64 * 100);
            bl.add_batch(
                ledger,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![entry],
                vec![],
                vec![],
            )
            .unwrap();

            // Verify ALL previous entries are still accessible
            for prev_ledger in 1..=ledger {
                let mut prev_id = [0u8; 32];
                prev_id[0..4].copy_from_slice(&prev_ledger.to_be_bytes());
                let key = make_account_key(prev_id);
                let found = bl.get(&key).unwrap();
                assert!(
                    found.is_some(),
                    "Entry from ledger {} not found at ledger {}",
                    prev_ledger,
                    ledger
                );
            }
        }
    }

    /// Regression test for memory leak fix: in-memory entries must be cleared
    /// when buckets move from level 0 to level 1.
    ///
    /// Without this fix, Arc<Vec<BucketEntry>> references would accumulate
    /// across bucket list generations, causing memory to grow at ~88 MB/hour.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_in_memory_entries_cleared_on_level0_spill() {
        let mut bl = BucketList::new();

        // Helper to create consistent IDs
        fn make_id(i: u32) -> [u8; 32] {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&i.to_be_bytes());
            id
        }

        // Add an entry at ledger 1 - this goes to level 0's curr
        let entry1 = make_account_entry(make_id(1), 100);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![entry1],
            vec![],
            vec![],
        )
        .unwrap();

        // Level 0's curr should have in-memory entries after add_batch
        assert!(
            bl.levels[0].curr.has_in_memory_entries(),
            "Level 0 curr should have in-memory entries after add_batch"
        );

        // Add at ledger 2 - this triggers level 0 to spill (snap)
        // The old curr moves to snap, and a new curr is created
        let entry2 = make_account_entry(make_id(2), 200);
        bl.add_batch(
            2,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![entry2],
            vec![],
            vec![],
        )
        .unwrap();

        // After ledger 2, level 0 has spilled:
        // - The bucket that was in curr is now in snap (and was passed to level 1)
        // - Level 0's snap should NOT have in-memory entries (memory leak fix)
        // - Level 0's curr should still have in-memory entries (for future merges)
        assert!(
            !bl.levels[0].snap.has_in_memory_entries(),
            "Level 0 snap should NOT have in-memory entries after spill (memory leak fix)"
        );
        assert!(
            bl.levels[0].curr.has_in_memory_entries(),
            "Level 0 curr should still have in-memory entries for future merges"
        );

        // Continue adding entries to trigger more spills and verify the pattern holds
        for ledger in 3..=8u32 {
            let entry = make_account_entry(make_id(ledger), ledger as i64 * 100);
            bl.add_batch(
                ledger,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![entry],
                vec![],
                vec![],
            )
            .unwrap();

            // At even ledgers, level 0 spills
            if ledger % 2 == 0 {
                assert!(
                    !bl.levels[0].snap.has_in_memory_entries(),
                    "Level 0 snap should NOT have in-memory entries after spill at ledger {}",
                    ledger
                );
            }

            // Level 0 curr should always have in-memory entries
            assert!(
                bl.levels[0].curr.has_in_memory_entries(),
                "Level 0 curr should have in-memory entries at ledger {}",
                ledger
            );
        }

        // Verify that entries are still accessible (functionality preserved)
        for i in 1..=8u32 {
            let key = make_account_key(make_id(i));
            assert!(
                bl.get(&key).unwrap().is_some(),
                "Entry {} should still be accessible",
                i
            );
        }
    }

    /// Regression test: Verify that AsyncMergeHandle tracks input file paths.
    ///
    /// This is critical for garbage collection correctness. Without tracking input files,
    /// the garbage collector could delete bucket files that are being read by an async merge,
    /// causing data corruption or panics.
    ///
    /// The fix adds `input_file_paths` to `AsyncMergeHandle` which are included in `referenced_file_paths()`.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_async_merge_handle_tracks_input_paths() {
        use crate::bucket::Bucket;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();

        // Create entries and save bucket to disk, then load it as disk-backed
        // We need larger buckets to exceed the disk-backed threshold,
        // so instead we'll use the lower-level Bucket::from_xdr_file_disk_backed API
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);

        // Create and save bucket 1
        let bucket1_mem = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry1)]).unwrap();
        let path1 = temp_dir.path().join("bucket1.xdr");
        bucket1_mem.save_to_xdr_file(&path1).unwrap();

        // Create and save bucket 2
        let bucket2_mem = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry2)]).unwrap();
        let path2 = temp_dir.path().join("bucket2.xdr");
        bucket2_mem.save_to_xdr_file(&path2).unwrap();

        // Load as disk-backed buckets
        let bucket1 = Arc::new(Bucket::from_xdr_file_disk_backed(&path1).unwrap());
        let bucket2 = Arc::new(Bucket::from_xdr_file_disk_backed(&path2).unwrap());

        // Verify buckets are disk-backed
        assert!(bucket1.is_disk_backed(), "bucket1 should be disk-backed");
        assert!(bucket2.is_disk_backed(), "bucket2 should be disk-backed");

        // Create an async merge handle
        let handle = AsyncMergeHandle::start_merge(AsyncMergeRequest {
            curr: bucket1.clone(),
            snap: bucket2.clone(),
            keep_dead_entries: false,
            protocol_version: TEST_PROTOCOL,
            normalize_init: true,
            shadow_buckets: vec![],
            level: 1,
            bucket_dir: Some(temp_dir.path().to_path_buf()),
            counters: None,
        });

        // Set up bucket list with the pending merge
        let mut bl = BucketList::new();
        bl.levels[1].next = Some(PendingMerge::Async(handle));

        // Get referenced_file_paths - it should include the async merge input files
        let paths = bl.referenced_file_paths();

        assert!(
            paths.contains(&path1),
            "referenced_file_paths should include first async merge input file. Paths: {:?}",
            paths
        );
        assert!(
            paths.contains(&path2),
            "referenced_file_paths should include second async merge input file. Paths: {:?}",
            paths
        );
    }

    // ============ P1-1: BucketList sizes at ledger 1 ============
    //
    // stellar-core: BucketListTests.cpp "BucketList sizes at ledger 1"
    // At ledger 1, level 0 curr should have exactly 1 entry,
    // all other buckets should be empty.

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bucket_list_sizes_at_ledger_1() {
        let mut bl = BucketList::new();

        // Add a single batch at ledger 1
        let entry = make_account_entry([1u8; 32], 100);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![entry],
            vec![],
            vec![],
        )
        .unwrap();

        // Level 0 curr should have entries (1 data entry + potentially metadata)
        let level0 = &bl.levels()[0];
        let level0_curr_data: usize = level0
            .curr
            .iter()
            .unwrap()
            .filter(|e| !e.as_ref().unwrap().is_metadata())
            .count();
        assert_eq!(
            level0_curr_data, 1,
            "Level 0 curr should have exactly 1 data entry at ledger 1"
        );

        // Level 0 snap should be empty
        assert!(
            level0.snap.is_empty(),
            "Level 0 snap should be empty at ledger 1"
        );

        // All other levels should be completely empty
        for level_idx in 1..BUCKET_LIST_LEVELS {
            let level = &bl.levels()[level_idx];
            assert!(
                level.curr.is_empty(),
                "Level {} curr should be empty at ledger 1",
                level_idx
            );
            assert!(
                level.snap.is_empty(),
                "Level {} snap should be empty at ledger 1",
                level_idx
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_hot_archive_bucket_list_sizes_at_ledger_1() {
        use crate::hot_archive::HotArchiveBucketList;

        let mut ha = HotArchiveBucketList::new();

        // Add a single archived entry at ledger 1
        let entry = make_account_entry([1u8; 32], 100);
        ha.add_batch(1, TEST_PROTOCOL, vec![entry], vec![]).unwrap();

        // Level 0 curr should have entries (1 data + potentially metadata)
        assert!(
            !ha.levels()[0].curr.is_empty(),
            "HA Level 0 curr should be non-empty at ledger 1"
        );

        // Level 0 snap should be empty
        assert!(
            ha.levels()[0].snap.is_empty(),
            "HA Level 0 snap should be empty at ledger 1"
        );

        // All other levels should be empty
        for level_idx in 1..crate::hot_archive::HOT_ARCHIVE_BUCKET_LIST_LEVELS {
            assert!(
                ha.levels()[level_idx].curr.is_empty(),
                "HA Level {} curr should be empty at ledger 1",
                level_idx
            );
            assert!(
                ha.levels()[level_idx].snap.is_empty(),
                "HA Level {} snap should be empty at ledger 1",
                level_idx
            );
        }
    }

    // ============ P1-2: BucketList iterative size check ============
    //
    // stellar-core: BucketListTests.cpp "BucketList check bucket sizes"
    // Validates that bucket entry counts match expected sizes across
    // many ledgers. Each ledger adds exactly one unique entry, so the
    // total entry count across all buckets should equal the ledger number.

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bucket_list_iterative_size_check() {
        let mut bl = BucketList::new();

        for ledger_seq in 1..=256u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&ledger_seq.to_be_bytes());
            let mut entry = make_account_entry(id, ledger_seq as i64 * 100);
            entry.last_modified_ledger_seq = ledger_seq;

            bl.add_batch(
                ledger_seq,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![entry],
                vec![],
                vec![],
            )
            .unwrap();

            // Count total non-metadata entries across all levels
            let mut total_data_entries: usize = 0;
            for level in bl.levels() {
                for entry_result in level.curr.iter().unwrap() {
                    let entry = entry_result.unwrap();
                    if !entry.is_metadata() {
                        total_data_entries += 1;
                    }
                }
                for entry_result in level.snap.iter().unwrap() {
                    let entry = entry_result.unwrap();
                    if !entry.is_metadata() {
                        total_data_entries += 1;
                    }
                }
            }

            // Total data entries across all buckets should equal ledger_seq
            // (one unique entry per ledger, no duplicates since keys are unique)
            assert_eq!(
                total_data_entries, ledger_seq as usize,
                "Total entries mismatch at ledger {}",
                ledger_seq
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bucket_list_entry_bounds_after_spills() {
        // After adding many entries, verify that lastModifiedLedgerSeq values
        // in each bucket respect level boundaries.
        let mut bl = BucketList::new();

        for ledger_seq in 1..=64u32 {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&ledger_seq.to_be_bytes());
            let mut entry = make_account_entry(id, ledger_seq as i64 * 100);
            entry.last_modified_ledger_seq = ledger_seq;

            bl.add_batch(
                ledger_seq,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![entry],
                vec![],
                vec![],
            )
            .unwrap();
        }

        // Check that entries at each level have reasonable lastModifiedLedgerSeq ranges
        for (level_idx, level) in bl.levels().iter().enumerate() {
            let mut curr_ledgers: Vec<u32> = Vec::new();
            for entry_result in level.curr.iter().unwrap() {
                let entry = entry_result.unwrap();
                if let Some(le) = entry.as_ledger_entry() {
                    curr_ledgers.push(le.last_modified_ledger_seq);
                }
            }
            let mut snap_ledgers: Vec<u32> = Vec::new();
            for entry_result in level.snap.iter().unwrap() {
                let entry = entry_result.unwrap();
                if let Some(le) = entry.as_ledger_entry() {
                    snap_ledgers.push(le.last_modified_ledger_seq);
                }
            }

            // Verify entries are contiguous within each bucket (no gaps)
            if curr_ledgers.len() > 1 {
                curr_ledgers.sort();
                let range = *curr_ledgers.last().unwrap() - *curr_ledgers.first().unwrap() + 1;
                assert_eq!(
                    range as usize,
                    curr_ledgers.len(),
                    "Level {} curr entries should be contiguous",
                    level_idx
                );
            }
            if snap_ledgers.len() > 1 {
                snap_ledgers.sort();
                let range = *snap_ledgers.last().unwrap() - *snap_ledgers.first().unwrap() + 1;
                assert_eq!(
                    range as usize,
                    snap_ledgers.len(),
                    "Level {} snap entries should be contiguous",
                    level_idx
                );
            }
        }
    }

    // ============ P1-3: Bucket list shadowing pre/post protocol 12 ============
    //
    // stellar-core: BucketListTests.cpp "bucket list shadowing pre/post proto 12"
    // Verifies that frequently-updated entries shadow correctly:
    // - Pre-protocol-12: entries shadowed at higher levels are filtered out
    // - Post-protocol-12: entries persist at all levels (no shadow filtering)

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bucket_list_lookup_shadowing_correctness() {
        // Add the same entry repeatedly with increasing balance. The most
        // recent value should always be returned by lookup, regardless of
        // how many levels it has propagated through.
        let mut bl = BucketList::new();

        let id = [0xAA; 32];
        for ledger_seq in 1..=200u32 {
            let entry = make_account_entry(id, ledger_seq as i64 * 100);
            bl.add_batch(
                ledger_seq,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![],
                vec![entry],
                vec![],
            )
            .unwrap();

            // After every add, lookup should return the latest value
            let key = make_account_key(id);
            let found = bl.get(&key).unwrap().unwrap();
            if let LedgerEntryData::Account(account) = &found.data {
                assert_eq!(
                    account.balance,
                    ledger_seq as i64 * 100,
                    "Lookup should return latest value at ledger {}",
                    ledger_seq
                );
            } else {
                panic!("Expected Account entry at ledger {}", ledger_seq);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bucket_list_shadowing_multiple_keys() {
        // Two frequently-updated entries (Alice and Bob) should always return
        // their latest values even as they propagate through multiple levels.
        let mut bl = BucketList::new();

        let alice_id = [0xAA; 32];
        let bob_id = [0xBB; 32];

        for ledger_seq in 1..=100u32 {
            let alice = make_account_entry(alice_id, ledger_seq as i64);
            let bob = make_account_entry(bob_id, ledger_seq as i64 * 10);

            bl.add_batch(
                ledger_seq,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![],
                vec![alice, bob],
                vec![],
            )
            .unwrap();
        }

        // Verify latest values
        let alice_key = make_account_key(alice_id);
        let bob_key = make_account_key(bob_id);

        let alice_entry = bl.get(&alice_key).unwrap().unwrap();
        let bob_entry = bl.get(&bob_key).unwrap().unwrap();

        if let LedgerEntryData::Account(a) = &alice_entry.data {
            assert_eq!(a.balance, 100, "Alice should have latest balance");
        }
        if let LedgerEntryData::Account(b) = &bob_entry.data {
            assert_eq!(b.balance, 1000, "Bob should have latest balance");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bucket_list_merge_records_counters() {
        let mut bl = BucketList::new();
        // Add enough batches to trigger spills (level 0 spills at ledger 2, 4, etc.)
        for seq in 1..=4u32 {
            let entry = make_account_entry([seq as u8; 32], seq as i64 * 100);
            bl.add_batch(
                seq,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![entry],
                vec![],
                vec![],
            )
            .unwrap();
        }

        let snap = bl.merge_counters().snapshot();
        // After 4 ledgers, level 0 should have spilled at least once, triggering merges.
        // The counters should reflect entries processed during those merges.
        assert!(
            snap.new_live_entries > 0 || snap.new_meta_entries > 0 || snap.new_init_entries > 0,
            "merge counters should record entries after spills: {:?}",
            snap
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_bucket_list_completed_merges_tracked() {
        let mut bl = BucketList::new();
        // Run enough ledgers to trigger an async merge (level >= 1)
        // Level 1 spills every 8 ledgers, but at ledger 2 level 0 spills which
        // triggers an in-memory merge. Async merges start at level 1+.
        for seq in 1..=8u32 {
            let entry = make_account_entry([seq as u8; 32], seq as i64 * 100);
            bl.add_batch(
                seq,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![entry],
                vec![],
                vec![],
            )
            .unwrap();
        }

        // drain_completed_merges should return merge keys and output hashes
        let completed = bl.drain_completed_merges();
        // After 8 ledgers, at least one async merge should have completed
        // (level 1 gets a merge when level 0 spills into it)
        assert!(
            !completed.is_empty(),
            "at least one merge should have been tracked after 8 ledgers"
        );

        // Each completed merge should have a non-zero output hash
        for (merge_key, output_hash) in &completed {
            assert!(
                !output_hash.is_zero(),
                "merge output hash should not be zero for key {:?}",
                merge_key
            );
        }

        // Draining again should be empty
        let completed2 = bl.drain_completed_merges();
        assert!(
            completed2.is_empty(),
            "drain should clear the completed merges"
        );
    }

    /// Regression test for VE-12: when an async merge produces an output whose
    /// canonical permanent file already exists but is corrupt (e.g. zero-byte
    /// due to a previous disk-full write failure), `start_merge` must detect the
    /// mismatch and replace the corrupt file with the freshly-computed output.
    ///
    /// Scenario that triggers the bug (without the fix):
    ///
    /// 1. An in-memory bucket with real entries and hash H is created.
    /// 2. `save_to_xdr_file` partially writes then fails, leaving a zero-byte
    ///    file at `{H}.bucket.xdr`.
    /// 3. A subsequent async merge whose result would also hash to H (e.g.
    ///    `merge(empty, bucket_H)`) checks the bucket dir, finds the existing
    ///    permanent file, loads it — getting hash = SHA-256("") — and uses that
    ///    corrupt bucket as the merge output.
    /// 4. The bucket list hash then includes SHA-256("") as L1.curr, diverging
    ///    from the expected value.
    ///
    /// After the fix, `start_merge` verifies the loaded bucket hash matches the
    /// expected value and replaces the file if it does not.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_start_merge_replaces_corrupt_permanent_file() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let bucket_dir = tmp.path().to_path_buf();

        // Step 1: Run an actual disk-backed merge to discover what hash
        // `merge(empty, snap_bucket)` produces, then corrupt that permanent file.
        let snap_entries = vec![
            BucketListEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketListEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];
        let snap_bucket = Arc::new(Bucket::from_entries(snap_entries).unwrap());
        assert!(!snap_bucket.is_empty());

        // Run a first merge to learn the real output hash.
        let mut level_probe = BucketLevel::new(1);
        level_probe
            .prepare_with_normalization(
                TEST_PROTOCOL,
                snap_bucket.clone(),
                &[],
                MergeContext {
                    keep_dead_entries: true,
                    normalize_init: false,
                    use_empty_curr: true, // merge(empty, snap_bucket)
                    bucket_dir: Some(&bucket_dir),
                    merge_map: None,
                    merge_counters: None,
                },
            )
            .unwrap();
        let _ = level_probe.commit().unwrap();
        let expected_hash = level_probe.curr.hash();
        assert!(
            !expected_hash.is_zero(),
            "first merge must produce a non-zero hash"
        );
        let empty_hash_hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_ne!(
            expected_hash.to_hex(),
            empty_hash_hex,
            "first merge must not produce SHA-256(empty)"
        );

        // Step 2: Corrupt the permanent file that was just created.
        let permanent_path = bucket_dir.join(canonical_bucket_filename(&expected_hash));
        assert!(
            permanent_path.exists(),
            "permanent file must exist after first merge"
        );
        std::fs::write(&permanent_path, b"").unwrap(); // truncate to zero bytes
        assert_eq!(
            std::fs::metadata(&permanent_path).unwrap().len(),
            0,
            "file must be zero bytes after corruption"
        );

        // Verify that loading the corrupt file gives the wrong hash.
        let corrupt_bucket = Bucket::from_xdr_file_disk_backed(&permanent_path).unwrap();
        assert_eq!(
            corrupt_bucket.hash().to_hex(),
            empty_hash_hex,
            "corrupt zero-byte file must hash to SHA-256(empty)"
        );

        // Step 3: Run the same merge again.  The permanent file at the expected
        // output path now exists but is corrupt.  The fix must detect this,
        // replace the file, and return the correct result.
        let mut level_fixed = BucketLevel::new(1);
        level_fixed
            .prepare_with_normalization(
                TEST_PROTOCOL,
                snap_bucket.clone(),
                &[],
                MergeContext {
                    keep_dead_entries: true,
                    normalize_init: false,
                    use_empty_curr: true,
                    bucket_dir: Some(&bucket_dir),
                    merge_map: None,
                    merge_counters: None,
                },
            )
            .unwrap();
        let _ = level_fixed.commit().unwrap();
        let result_hash = level_fixed.curr.hash();

        // The result must be the correct hash, not SHA-256("").
        assert_ne!(
            result_hash.to_hex(),
            empty_hash_hex,
            "merge must not return SHA-256(empty) when permanent file is corrupt"
        );
        assert_eq!(
            result_hash, expected_hash,
            "merge must return the same hash as the first (uncorrupted) run"
        );

        // The permanent file must have been replaced with valid content.
        let reloaded = Bucket::from_xdr_file_disk_backed(&permanent_path).unwrap();
        assert_eq!(
            reloaded.hash(),
            expected_hash,
            "permanent file must contain valid bucket content after fix"
        );
    }

    // ============ restore_from_has_parallel tests ============

    /// Build a HashMap-backed loader closure for tests.
    fn make_loader(
        buckets: Vec<Bucket>,
    ) -> impl Fn(&Hash256) -> crate::Result<Bucket> + Send + Sync {
        let map: std::collections::HashMap<Hash256, Bucket> =
            buckets.into_iter().map(|b| (b.hash(), b)).collect();
        let map = std::sync::Arc::new(map);
        move |hash: &Hash256| {
            map.get(hash).cloned().ok_or_else(|| {
                BucketError::Serialization(format!("bucket not found: {}", hash.to_hex()))
            })
        }
    }

    #[test]
    fn test_restore_from_has_parallel_basic_curr_snap() {
        // Two levels have non-empty curr/snap; rest are empty.
        // Verify entries are accessible via get() after parallel restore.
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);

        let bucket0_curr = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry1)]).unwrap();
        let bucket1_curr = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry2)]).unwrap();

        let h0c = bucket0_curr.hash();
        let h1c = bucket1_curr.hash();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[0] = (h0c, Hash256::ZERO);
        hashes[1] = (h1c, Hash256::ZERO);

        let next_states = vec![HasNextState::default(); BUCKET_LIST_LEVELS];

        let loader = make_loader(vec![bucket0_curr, bucket1_curr]);
        let bl = BucketList::restore_from_has_parallel(&hashes, &next_states, loader).unwrap();

        assert_eq!(bl.levels().len(), BUCKET_LIST_LEVELS);

        let key1 = make_account_key([1u8; 32]);
        assert!(
            bl.get(&key1).unwrap().is_some(),
            "entry1 should be found after parallel restore"
        );

        let key2 = make_account_key([2u8; 32]);
        assert!(
            bl.get(&key2).unwrap().is_some(),
            "entry2 should be found after parallel restore"
        );

        // Level 2 should be empty
        assert!(bl.levels()[2].curr.is_empty());
        assert!(bl.levels()[2].snap.is_empty());
    }

    #[test]
    fn test_restore_from_has_parallel_output_bucket_loaded_as_next() {
        // When a level has HAS_NEXT_STATE_OUTPUT, the output bucket must be
        // loaded into level.next as PendingMerge::InMemory.
        let entry_curr = make_account_entry([1u8; 32], 100);
        let entry_out = make_account_entry([9u8; 32], 999);

        let bucket_curr =
            Bucket::from_entries(vec![BucketListEntry::Liveentry(entry_curr)]).unwrap();
        let bucket_out = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry_out)]).unwrap();

        let hc = bucket_curr.hash();
        let ho = bucket_out.hash();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[0] = (hc, Hash256::ZERO);

        let mut next_states = vec![HasNextState::default(); BUCKET_LIST_LEVELS];
        next_states[0] = HasNextState {
            state: HAS_NEXT_STATE_OUTPUT,
            output: Some(ho),
            input_curr: None,
            input_snap: None,
        };

        let loader = make_loader(vec![bucket_curr, bucket_out]);
        let bl = BucketList::restore_from_has_parallel(&hashes, &next_states, loader).unwrap();

        assert!(
            bl.levels()[0].next.is_some(),
            "level 0 should have a pending merge output (HAS_NEXT_STATE_OUTPUT)"
        );
        // Other levels have no next
        for i in 1..BUCKET_LIST_LEVELS {
            assert!(
                bl.levels()[i].next.is_none(),
                "level {} should have no next",
                i
            );
        }
    }

    #[test]
    fn test_restore_from_has_parallel_clear_state_no_next() {
        // HAS_NEXT_STATE_CLEAR (default) means no level.next should be set.
        let entry = make_account_entry([5u8; 32], 500);
        let bucket = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry)]).unwrap();
        let h = bucket.hash();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[3] = (h, Hash256::ZERO);

        let next_states = vec![HasNextState::default(); BUCKET_LIST_LEVELS]; // all CLEAR

        let loader = make_loader(vec![bucket]);
        let bl = BucketList::restore_from_has_parallel(&hashes, &next_states, loader).unwrap();

        for i in 0..BUCKET_LIST_LEVELS {
            assert!(
                bl.levels()[i].next.is_none(),
                "level {} should have no next for CLEAR state",
                i
            );
        }
    }

    #[test]
    fn test_restore_from_has_parallel_matches_sequential() {
        // restore_from_has_parallel must produce results identical to restore_from_has
        // for the same inputs: same entries retrievable, same level.next presence.
        let entries: Vec<LedgerEntry> = (1u8..=4)
            .map(|i| make_account_entry([i; 32], i as i64 * 100))
            .collect();

        let b0c =
            Bucket::from_entries(vec![BucketListEntry::Liveentry(entries[0].clone())]).unwrap();
        let b0s =
            Bucket::from_entries(vec![BucketListEntry::Liveentry(entries[1].clone())]).unwrap();
        let b2c =
            Bucket::from_entries(vec![BucketListEntry::Liveentry(entries[2].clone())]).unwrap();
        let bout =
            Bucket::from_entries(vec![BucketListEntry::Liveentry(entries[3].clone())]).unwrap();

        let h0c = b0c.hash();
        let h0s = b0s.hash();
        let h2c = b2c.hash();
        let hout = bout.hash();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[0] = (h0c, h0s);
        hashes[2] = (h2c, Hash256::ZERO);

        // Level 1 has a completed merge output
        let mut next_states = vec![HasNextState::default(); BUCKET_LIST_LEVELS];
        next_states[1] = HasNextState {
            state: HAS_NEXT_STATE_OUTPUT,
            output: Some(hout),
            input_curr: None,
            input_snap: None,
        };

        let all_buckets = vec![b0c, b0s, b2c, bout];

        let loader_seq = make_loader(all_buckets.clone());
        let bl_seq =
            BucketList::restore_from_has(&hashes, &next_states, |h| loader_seq(h)).unwrap();

        let loader_par = make_loader(all_buckets);
        let bl_par =
            BucketList::restore_from_has_parallel(&hashes, &next_states, loader_par).unwrap();

        // All entries should be reachable from both
        for i in 1u8..=4 {
            let key = make_account_key([i; 32]);
            let seq_result = bl_seq.get(&key).unwrap();
            let par_result = bl_par.get(&key).unwrap();
            assert_eq!(
                seq_result.is_some(),
                par_result.is_some(),
                "entry [{}; 32] presence mismatch between sequential and parallel restore",
                i
            );
        }

        // level.next presence must match
        for i in 0..BUCKET_LIST_LEVELS {
            assert_eq!(
                bl_seq.levels()[i].next.is_some(),
                bl_par.levels()[i].next.is_some(),
                "level {} next presence mismatch between sequential and parallel restore",
                i
            );
        }
    }

    #[test]
    fn test_restore_from_has_parallel_wrong_level_count_errors() {
        // Passing wrong number of levels must return an error.
        let next_states = vec![HasNextState::default(); BUCKET_LIST_LEVELS];
        let too_short = vec![(Hash256::ZERO, Hash256::ZERO); 5]; // < BUCKET_LIST_LEVELS

        let result = BucketList::restore_from_has_parallel(&too_short, &next_states, |_| {
            unreachable!("should not call loader")
        });
        assert!(result.is_err(), "should return error for wrong level count");
    }

    #[test]
    fn test_perform_merge_replaces_corrupt_existing_permanent_file() {
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let bucket_dir = tmp.path().to_path_buf();

        let snap_entries = vec![
            BucketListEntry::Liveentry(make_account_entry([1u8; 32], 100)),
            BucketListEntry::Liveentry(make_account_entry([2u8; 32], 200)),
        ];
        let snap_bucket = Bucket::from_entries(snap_entries).unwrap();
        let empty_curr = Bucket::empty();

        // First run creates a valid permanent file.
        let first = perform_merge(
            &empty_curr,
            &snap_bucket,
            Some(&bucket_dir),
            true,
            TEST_PROTOCOL,
        )
        .unwrap();
        let expected_hash = first.hash();
        assert!(!expected_hash.is_zero());

        let permanent_path = bucket_dir.join(canonical_bucket_filename(&expected_hash));
        assert!(permanent_path.exists());

        // Corrupt the permanent file to simulate prior partial write.
        std::fs::write(&permanent_path, b"").unwrap();
        assert_eq!(std::fs::metadata(&permanent_path).unwrap().len(), 0);

        // Second run should detect mismatch and replace the file, not trust it.
        let second = perform_merge(
            &empty_curr,
            &snap_bucket,
            Some(&bucket_dir),
            true,
            TEST_PROTOCOL,
        )
        .unwrap();
        assert_eq!(second.hash(), expected_hash);

        let reloaded = Bucket::from_xdr_file_disk_backed(&permanent_path).unwrap();
        assert_eq!(reloaded.hash(), expected_hash);
    }

    /// Regression test for AUDIT-BC2: commit() must propagate merge errors
    /// instead of silently keeping the old bucket.
    ///
    /// Previously, commit() caught async merge errors, logged them, and
    /// returned None — silently keeping the stale curr bucket. This meant
    /// a failed merge would leave the bucket list in an incorrect state,
    /// causing bucket list hash divergence and consensus failure.
    ///
    /// stellar-core behavior: FutureBucket::resolve() propagates exceptions
    /// from the background merge thread. BucketLevel::commit() does not catch
    /// them — they crash the node via releaseAssert or unhandled exception.
    /// Merge failures are always fatal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_audit_bc2_commit_propagates_merge_error() {
        use tokio::sync::oneshot;

        // Create a level with a known curr bucket
        let mut level = BucketLevel::new(1);

        // Set up a PendingMerge::Async with a channel that sends an error
        let (sender, receiver) = oneshot::channel();
        let merge_key = MergeKey::new(true, Hash256::default(), Hash256::default());

        let handle = AsyncMergeHandle {
            receiver: Some(receiver),
            level: 1,
            result: None,
            input_file_paths: vec![],
            input_curr_hash: Hash256::default(),
            input_snap_hash: Hash256::default(),
            merge_key,
        };

        level.next = Some(PendingMerge::Async(handle));

        // Send an error through the channel (simulates a failed background merge)
        sender
            .send(Err(BucketError::Merge(
                "simulated merge failure".to_string(),
            )))
            .unwrap();

        // commit() must return Err, NOT silently keep the old bucket
        let result = level.commit();
        assert!(
            result.is_err(),
            "commit() must propagate merge errors, not silently keep old bucket"
        );
    }

    /// Regression test for AUDIT-BC2: resolve_pending_merge() must propagate errors.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_audit_bc2_resolve_pending_merge_propagates_error() {
        use tokio::sync::oneshot;

        let mut level = BucketLevel::new(1);

        let (sender, receiver) = oneshot::channel();
        let merge_key = MergeKey::new(true, Hash256::default(), Hash256::default());

        let handle = AsyncMergeHandle {
            receiver: Some(receiver),
            level: 1,
            result: None,
            input_file_paths: vec![],
            input_curr_hash: Hash256::default(),
            input_snap_hash: Hash256::default(),
            merge_key,
        };

        level.next = Some(PendingMerge::Async(handle));

        // Send an error
        sender
            .send(Err(BucketError::Merge(
                "simulated merge failure".to_string(),
            )))
            .unwrap();

        // resolve_pending_merge() must return Err
        let result = level.resolve_pending_merge();
        assert!(
            result.is_err(),
            "resolve_pending_merge() must propagate merge errors"
        );
    }
}
