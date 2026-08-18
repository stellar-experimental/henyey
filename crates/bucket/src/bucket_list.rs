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

use henyey_common::{BucketListDbConfig, Hash256};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use stellar_xdr::{
    BucketListType, BucketMetadata, BucketMetadataExt, LedgerEntry, LedgerKey, Limits,
    StateArchivalSettings, WriteXdr,
};

// Re-import oneshot for AsyncMergeHandle (needed for Sync on BucketList)
use tokio::sync::oneshot;

use crate::bucket::Bucket;
use crate::cache::CacheStats;
use crate::entry::{get_ttl_key, is_ttl_expired, BucketEntry, BucketEntryExt};
use crate::eviction::{
    eviction_scan_is_stuck, update_starting_eviction_iterator, EvictionCandidate, EvictionIterator,
    EvictionIteratorExt, EvictionResult,
};
use crate::future_bucket::MergeKey;
use crate::index::BucketEntryCounters;
use crate::live_iterator::LiveEntriesIterator;
use crate::manager::{canonical_bucket_filename, promote_temp_to_canonical, temp_merge_path};
use crate::merge::{
    merge_buckets, merge_buckets_to_file, merge_in_memory, DeadEntryPolicy, InitEntryPolicy,
    MergeOptions, MetadataPolicy,
};
use crate::merge_map::BucketMergeMap;
use crate::metrics::{EvictionCounters, MergeCounters};
use crate::{
    protocol_version_is_before, protocol_version_starts_from, BucketError, ProtocolVersion, Result,
};
use henyey_common::{is_persistent_entry, is_soroban_entry, is_temporary_entry};

/// Number of levels in the BucketList (matches stellar-core's `kNumLevels`).
pub const BUCKET_LIST_LEVELS: usize = 11;

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

/// Pending merge result for a bucket level.
///
/// This supports two modes matching stellar-core:
/// - `InMemory`: Synchronous merge result (used for level 0)
/// - `Async`: Background merge in progress (used for levels 1+)
/// - `Shared`: Reattached to an in-flight merge started elsewhere
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
    /// Reattached to an in-flight merge started elsewhere.
    /// Used for in-flight merge dedup (stellar-core getMergeFuture parity).
    Shared(SharedMergeHandle),
}

/// Describes the serializable state of a pending merge for HAS persistence.
///
/// Matches the three states of stellar-core FutureBucket:
/// - State 0 (clear): no pending merge (represented by `None` at call site)
/// - State 1 (output): merge completed, output hash known
/// - State 2 (inputs): merge in progress, input hashes known
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingMergeState {
    /// State 1: merge completed, output bucket hash is known.
    /// Invariant: the hash is guaranteed non-zero (zero-hash outputs are
    /// canonicalized to `None` at parse time).
    Output(Hash256),
    /// State 2: merge in progress, input curr/snap hashes are known
    Inputs { curr: Hash256, snap: Hash256 },
}

impl PendingMergeState {
    /// All bucket hashes referenced by this merge state.
    /// Invariant: all returned hashes are non-zero (enforced by parse-time canonicalization).
    pub fn referenced_hashes(&self) -> impl Iterator<Item = &Hash256> {
        match self {
            PendingMergeState::Output(h) => [Some(h), None].into_iter().flatten(),
            PendingMergeState::Inputs { curr, snap } => {
                [Some(curr), Some(snap)].into_iter().flatten()
            }
        }
    }
}

impl std::fmt::Debug for PendingMerge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PendingMerge::InMemory(b) => f
                .debug_struct("InMemory")
                .field("hash", &b.hash().to_hex())
                .finish(),
            PendingMerge::Async(h) => f.debug_struct("Async").field("level", &h.level).finish(),
            PendingMerge::Shared(h) => f
                .debug_struct("Shared")
                .field("level", &h.metadata.level)
                .finish(),
        }
    }
}

impl PendingMerge {
    /// Get the hash of the pending merge result.
    ///
    /// For InMemory, returns the bucket hash directly.
    /// For Async, returns the cached result hash if resolved, otherwise returns a placeholder.
    /// For Shared, returns the cached result hash if resolved, otherwise zero.
    pub fn hash(&self) -> Hash256 {
        match self {
            PendingMerge::InMemory(bucket) => bucket.hash(),
            PendingMerge::Async(handle) => {
                // If we have a cached result, return its hash
                if let MergeRecvState::Ready(Ok(ref bucket)) = handle.state {
                    bucket.hash()
                } else {
                    // Return zero hash to indicate unresolved or failed async merge
                    Hash256::default()
                }
            }
            PendingMerge::Shared(handle) => {
                if let Some(Ok(ref bucket)) = handle.cached_result {
                    bucket.hash()
                } else {
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
    pub keep_dead_entries: DeadEntryPolicy,
    /// If true, convert INIT entries to LIVE entries during the merge.
    /// Should ALWAYS be false in production; exists for test compatibility.
    pub normalize_init: InitEntryPolicy,
    /// If true, use an empty bucket instead of the level's actual curr.
    pub use_empty_curr: bool,
    pub bucket_dir: Option<&'a std::path::Path>,
    pub merge_map: Option<&'a std::sync::Arc<std::sync::RwLock<BucketMergeMap>>>,
    pub merge_counters: Option<Arc<MergeCounters>>,
    /// BucketListDB config governing the index layout of disk-backed merge
    /// outputs. Sourced from `BucketList::bucket_list_db_config` (default when
    /// unset). Merge outputs are re-indexed on the next `load_bucket`, so this
    /// only sizes the transient promotion-time index; threaded for load-path parity.
    pub bucket_list_db: BucketListDbConfig,
}

struct AsyncMergeRequest {
    curr: Arc<Bucket>,
    snap: Arc<Bucket>,
    keep_dead_entries: DeadEntryPolicy,
    protocol_version: u32,
    normalize_init: InitEntryPolicy,
    shadow_buckets: Vec<Bucket>,
    level: usize,
    bucket_dir: Option<std::path::PathBuf>,
    counters: Option<Arc<MergeCounters>>,
    /// BucketListDB config governing the index layout of the merge output when
    /// promoted to a disk-backed bucket. The merge output is re-indexed on the
    /// next `BucketManager::load_bucket` regardless, so this only affects the
    /// transient index built at promotion time; it is threaded for parity with
    /// the load path. Defaults to `BucketListDbConfig::default()`.
    bucket_list_db: BucketListDbConfig,
}

struct AddBatchArgs {
    ledger_seq: u32,
    protocol_version: u32,
    bucket_list_type: BucketListType,
    init_entries: Vec<LedgerEntry>,
    live_entries: Vec<LedgerEntry>,
    dead_entries: Vec<LedgerKey>,
}

/// State of the merge result receiver.
///
/// Uses an enum to make the invalid "consumed" state (receiver gone, no result)
/// unrepresentable. After any outcome (success, error, cancellation), transitions
/// to `Ready` and caches the result for idempotent access.
enum MergeRecvState {
    /// Merge in progress; receiver delivers the result.
    Pending(oneshot::Receiver<Result<Bucket>>),
    /// Terminal state — merge completed or failed; result is cached.
    ///
    /// Errors are stored as a structured [`MergeError`] (not a bare string)
    /// since `BucketError` is not `Clone`. The `MergeError` carries the coarse
    /// class / raw `errno`, so `resolve()` can re-materialize a *classified*
    /// `BucketError` and a transient ENOSPC stays distinguishable from
    /// corruption (#3478).
    Ready(std::result::Result<Arc<Bucket>, crate::merge_map::MergeError>),
}

/// Handle to an asynchronous bucket merge running in a background thread.
///
/// The merge is started immediately when this handle is created, and runs
/// concurrently with other operations. Call `resolve()` to wait for completion.
///
/// Uses `tokio::sync::oneshot` for the channel with a runtime-aware blocking
/// helper (`blocking_recv_oneshot`) so that `resolve()` works on any tokio
/// runtime flavor (multi-thread, current-thread) as well as outside of any
/// runtime. The merge task runs on `spawn_blocking` (a separate OS thread).
pub struct AsyncMergeHandle {
    /// Merge result state — either pending or resolved (success/failure).
    state: MergeRecvState,
    /// The level this merge is for (for logging/debugging).
    level: usize,
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
    /// Panics if called outside of a tokio runtime context.
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
            bucket_list_db,
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
                match merge_buckets_to_file(
                    &curr,
                    &snap,
                    &temp_path,
                    &MergeOptions {
                        keep_dead_entries,
                        max_protocol_version: protocol_version,
                        normalize_init_entries: normalize_init,
                        counters: counters_ref,
                        ..Default::default()
                    },
                ) {
                    Ok((hash, entry_count)) => {
                        if entry_count == 0 {
                            let _ = std::fs::remove_file(&temp_path);
                            Ok(Bucket::empty())
                        } else {
                            promote_temp_to_canonical(
                                &temp_path,
                                dir,
                                &hash,
                                "start_merge",
                                &bucket_list_db,
                            )
                        }
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&temp_path);
                        Err(e)
                    }
                }
            } else {
                // In-memory merge (used in tests or when no bucket_dir is set)
                merge_buckets(
                    &curr,
                    &snap,
                    &MergeOptions {
                        keep_dead_entries,
                        max_protocol_version: protocol_version,
                        normalize_init_entries: normalize_init,
                        shadow_buckets: &shadow_buckets,
                        counters: counters_ref,
                        ..Default::default()
                    },
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
            state: MergeRecvState::Pending(receiver),
            level,
            input_file_paths,
            input_curr_hash,
            input_snap_hash,
            merge_key,
        }
    }

    /// Resolve the merge, blocking until complete if necessary.
    ///
    /// After calling this, the result is cached and can be retrieved multiple times.
    /// Both success and failure are cached — subsequent calls return the same result.
    ///
    /// Uses a runtime-aware blocking strategy:
    /// - Multi-thread runtime: `block_in_place` + `blocking_recv`
    /// - Current-thread runtime: helper thread (avoids blocking_recv panic in async context)
    /// - No runtime: `blocking_recv` directly
    pub fn resolve(&mut self) -> Result<Arc<Bucket>> {
        if let MergeRecvState::Ready(ref result) = self.state {
            // Re-materialize a *classified* BucketError from the cached
            // structured MergeError so the errno survives repeated resolves.
            return result.clone().map_err(|e| e.to_bucket_error());
        }

        let start = std::time::Instant::now();

        // Take the Pending receiver, replacing with a temporary Ready(Err) in case
        // the recv panics (belt-and-suspenders). Classified `Other` (fatal).
        let MergeRecvState::Pending(rx) = std::mem::replace(
            &mut self.state,
            MergeRecvState::Ready(Err(crate::merge_map::MergeError::from_message(
                "merge resolve interrupted",
            ))),
        ) else {
            unreachable!("already checked for Ready above");
        };

        let recv_result = blocking_recv_oneshot(rx);

        match recv_result {
            Ok(bucket_result) => match bucket_result {
                Ok(bucket) => {
                    let elapsed = start.elapsed();
                    if elapsed.as_millis() > 10 {
                        tracing::info!(
                            level = self.level,
                            wait_ms = elapsed.as_millis(),
                            "Waited for background merge to complete"
                        );
                    }
                    let bucket = Arc::new(bucket);
                    self.state = MergeRecvState::Ready(Ok(bucket.clone()));
                    Ok(bucket)
                }
                Err(e) => {
                    // Preserve the structured class/errno from the original
                    // BucketError (#3478) instead of stringifying it away.
                    let merge_err = crate::merge_map::MergeError::from_bucket_error(&e);
                    self.state = MergeRecvState::Ready(Err(merge_err.clone()));
                    Err(merge_err.to_bucket_error())
                }
            },
            Err(e) => {
                // Channel/cancellation error — no originating BucketError class.
                let merge_err = crate::merge_map::MergeError::from_bucket_error(&e);
                self.state = MergeRecvState::Ready(Err(merge_err.clone()));
                Err(merge_err.to_bucket_error())
            }
        }
    }

    /// Non-blocking counterpart to [`resolve`](Self::resolve).
    ///
    /// If the background merge has already finished, cache its result so a later
    /// `resolve()` (i.e. the boundary `commit()`) takes the instant `Ready` fast
    /// path instead of parking the event loop for the rest of a deep-level merge
    /// (#3867). If the merge is still running, leave the handle `Pending` and
    /// return immediately — this NEVER blocks. Idempotent: a no-op on an
    /// already-`Ready` handle.
    ///
    /// Success and both failure modes are cached using the EXACT same
    /// classification as `resolve()` so the eager (poll) and lazy (resolve)
    /// observation paths can never diverge — in particular the `Closed`
    /// (cancelled-merge) arm reproduces `resolve()`'s
    /// `BucketError::Merge("merge task was cancelled")` before wrapping it in a
    /// structured [`MergeError`], preserving #3478 errno classification.
    pub fn try_resolve(&mut self) {
        // Already resolved (success or failure): nothing to do, idempotent.
        if matches!(self.state, MergeRecvState::Ready(_)) {
            return;
        }

        // Non-blocking peek. The borrow of `self.state` ends when `try_recv`
        // returns an owned value, so the assignments below are free to replace
        // `self.state`.
        let recv = match self.state {
            MergeRecvState::Pending(ref mut rx) => rx.try_recv(),
            MergeRecvState::Ready(_) => unreachable!("already checked for Ready above"),
        };

        match recv {
            Ok(Ok(bucket)) => {
                self.state = MergeRecvState::Ready(Ok(Arc::new(bucket)));
            }
            Ok(Err(e)) => {
                // Preserve the structured class/errno exactly as resolve() does
                // (bucket_list.rs resolve() Err arm, #3478).
                let merge_err = crate::merge_map::MergeError::from_bucket_error(&e);
                self.state = MergeRecvState::Ready(Err(merge_err));
            }
            Err(oneshot::error::TryRecvError::Empty) => {
                // Merge still running — leave Pending. Never blocks.
            }
            Err(oneshot::error::TryRecvError::Closed) => {
                // Sender dropped without sending. resolve() maps a closed channel
                // (blocking_recv_oneshot's RecvError) to
                // BucketError::Merge("merge task was cancelled"); classify it
                // identically here so the two paths cannot diverge.
                let merge_err = crate::merge_map::MergeError::from_bucket_error(
                    &BucketError::Merge("merge task was cancelled".to_string()),
                );
                self.state = MergeRecvState::Ready(Err(merge_err));
            }
        }
    }

    /// Test-only constructor: build a handle already resolved to a failure
    /// carrying a structured [`MergeError`] (with its real `errno`).
    ///
    /// Used by the #3478 regression tests to inject the exact production
    /// ENOSPC failure so the classification path (`is_transient_io`) is
    /// exercised on a real structured errno, not a hard-coded boolean.
    #[cfg(test)]
    pub(crate) fn new_failed_for_test(level: usize, error: crate::merge_map::MergeError) -> Self {
        let merge_key = MergeKey::new(
            DeadEntryPolicy::Keep,
            Hash256::default(),
            Hash256::default(),
        );
        Self {
            state: MergeRecvState::Ready(Err(error)),
            level,
            input_file_paths: Vec::new(),
            input_curr_hash: Hash256::default(),
            input_snap_hash: Hash256::default(),
            merge_key,
        }
    }
}

/// Receive from a tokio oneshot channel using the appropriate blocking strategy
/// for the current runtime context.
///
/// - Multi-thread runtime: `block_in_place(|| receiver.blocking_recv())`
/// - Current-thread runtime: spawns a helper thread (blocking_recv panics in async context)
/// - No runtime: `receiver.blocking_recv()` directly
fn blocking_recv_oneshot(
    receiver: oneshot::Receiver<Result<Bucket>>,
) -> std::result::Result<Result<Bucket>, BucketError> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            if matches!(
                handle.runtime_flavor(),
                tokio::runtime::RuntimeFlavor::MultiThread
            ) {
                tokio::task::block_in_place(|| {
                    receiver
                        .blocking_recv()
                        .map_err(|_| BucketError::Merge("merge task was cancelled".to_string()))
                })
            } else {
                // Current-thread runtime: blocking_recv panics in async context,
                // so hop to a helper thread. The merge runs on spawn_blocking
                // (separate OS thread), so the channel will still unblock.
                std::thread::spawn(move || {
                    receiver
                        .blocking_recv()
                        .map_err(|_| BucketError::Merge("merge task was cancelled".to_string()))
                })
                .join()
                .map_err(|_| BucketError::Merge("merge helper thread panicked".to_string()))?
            }
        }
        Err(_) => {
            // No runtime — blocking_recv is safe outside async context
            receiver
                .blocking_recv()
                .map_err(|_| BucketError::Merge("merge task was cancelled".to_string()))
        }
    }
}

// ============================================================================
// SharedMergeHandle — consumer side of in-flight merge dedup
// ============================================================================

use crate::merge_map::{MergeResult, SharedMergeMetadata};
use tokio::sync::watch;

/// Handle to a shared in-flight merge. Created when a merge reattaches to
/// an existing running merge (in-flight dedup).
///
/// The consumer side: resolve() blocks until the producer signals completion.
/// Does NOT own the merge lifetime — dropping this handle does not cancel the merge.
pub struct SharedMergeHandle {
    receiver: watch::Receiver<Option<MergeResult>>,
    pub(crate) metadata: SharedMergeMetadata,
    pub(crate) cached_result: Option<MergeResult>,
}

impl SharedMergeHandle {
    /// Create a new shared merge handle.
    pub fn new(
        receiver: watch::Receiver<Option<MergeResult>>,
        metadata: SharedMergeMetadata,
    ) -> Self {
        Self {
            receiver,
            metadata,
            cached_result: None,
        }
    }

    /// Resolve the shared merge, blocking until the result is available.
    ///
    /// Uses the same runtime-aware blocking strategy as AsyncMergeHandle::resolve().
    pub fn resolve(&mut self) -> Result<Arc<Bucket>> {
        if let Some(ref result) = self.cached_result {
            // Re-materialize a classified BucketError from the structured
            // MergeError so the errno survives (#3478).
            return result.clone().map_err(|e| e.to_bucket_error());
        }

        let result = blocking_recv_watch(&mut self.receiver)?;
        self.cached_result = Some(result.clone());
        result.map_err(|e| e.to_bucket_error())
    }

    /// Non-blocking counterpart to [`resolve`](Self::resolve). If the shared
    /// merge has already published its result, cache it so the boundary
    /// `commit()` takes the fast path; otherwise leave the handle unresolved and
    /// return immediately. NEVER blocks. Idempotent on an already-cached handle.
    pub fn try_resolve(&mut self) {
        if self.cached_result.is_some() {
            return;
        }
        // watch::Receiver::borrow() reads the current value without waiting.
        let current = self.receiver.borrow().clone();
        if let Some(result) = current {
            self.cached_result = Some(result);
        }
    }
}

/// Block on a watch receiver until the value becomes Some, using the same
/// runtime-detection strategy as blocking_recv_oneshot.
fn blocking_recv_watch(receiver: &mut watch::Receiver<Option<MergeResult>>) -> Result<MergeResult> {
    // Check if value is already available
    {
        let current = receiver.borrow().clone();
        if let Some(result) = current {
            return Ok(result);
        }
    }

    // Block until value changes to Some
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            if matches!(
                handle.runtime_flavor(),
                tokio::runtime::RuntimeFlavor::MultiThread
            ) {
                tokio::task::block_in_place(|| {
                    // Use blocking wait for watch
                    loop {
                        if receiver.has_changed().unwrap_or(true) {
                            let val = receiver.borrow_and_update().clone();
                            if let Some(result) = val {
                                return Ok(result);
                            }
                        }
                        // Check if sender is dropped
                        if receiver.has_changed().is_err() {
                            return Err(BucketError::Merge(
                                "shared merge channel closed".to_string(),
                            ));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                })
            } else {
                // Current-thread runtime: use helper thread
                let mut rx = receiver.clone();
                let result = std::thread::spawn(move || -> Result<MergeResult> {
                    loop {
                        if rx.has_changed().unwrap_or(true) {
                            let val = rx.borrow_and_update().clone();
                            if let Some(result) = val {
                                return Ok(result);
                            }
                        }
                        if rx.has_changed().is_err() {
                            return Err(BucketError::Merge(
                                "shared merge channel closed".to_string(),
                            ));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                })
                .join()
                .map_err(|_| {
                    BucketError::Merge("shared merge helper thread panicked".to_string())
                })??;
                Ok(result)
            }
        }
        Err(_) => {
            // No runtime — poll directly
            loop {
                if receiver.has_changed().unwrap_or(true) {
                    let val = receiver.borrow_and_update().clone();
                    if let Some(result) = val {
                        return Ok(result);
                    }
                }
                if receiver.has_changed().is_err() {
                    return Err(BucketError::Merge(
                        "shared merge channel closed".to_string(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
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
    /// Guard for in-flight merge dedup tracking. When a new merge is started
    /// via get_or_start, this guard signals completion to any reattached consumers.
    in_flight_guard: Option<crate::merge_map::InFlightGuard>,
}

impl BucketLevel {
    /// Create a new empty level.
    pub fn new(level: usize) -> Self {
        Self {
            curr: Arc::new(Bucket::empty()),
            snap: Arc::new(Bucket::empty()),
            next: None,
            level,
            in_flight_guard: None,
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
        Hash256::from_sha256(hasher)
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
                    // Guard (if any) was already signaled in prepare for sync merges
                    self.in_flight_guard = None;
                    Ok(None)
                }
                PendingMerge::Async(mut handle) => {
                    let merge_key = handle.merge_key.clone();
                    let bucket = handle.resolve()?;
                    let output_hash = bucket.hash();
                    // Signal in-flight guard so reattached consumers get the result
                    if let Some(guard) = self.in_flight_guard.take() {
                        guard.complete(Arc::clone(&bucket));
                    }
                    self.curr = bucket;
                    Ok(Some((merge_key, output_hash)))
                }
                PendingMerge::Shared(mut handle) => {
                    // Resolve the shared merge. The InFlightGuard (owned by the
                    // producer) already recorded in the completed cache, so we
                    // return None to avoid double-recording.
                    let bucket = handle.resolve()?;
                    self.curr = bucket;
                    self.in_flight_guard = None;
                    Ok(None)
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
                if let MergeRecvState::Ready(Ok(ref bucket)) = handle.state {
                    // Async merge has resolved; emit state 1 (output)
                    let h = bucket.hash();
                    if h.is_zero() {
                        None
                    } else {
                        Some(PendingMergeState::Output(h))
                    }
                } else {
                    // Async merge still in progress or failed; emit state 2 (input hashes)
                    Some(PendingMergeState::Inputs {
                        curr: handle.input_curr_hash,
                        snap: handle.input_snap_hash,
                    })
                }
            }
            Some(PendingMerge::Shared(handle)) => {
                if let Some(Ok(ref bucket)) = handle.cached_result {
                    // Shared merge resolved; emit state 1 (output) if non-empty
                    let h = bucket.hash();
                    if h.is_zero() {
                        None
                    } else {
                        Some(PendingMergeState::Output(h))
                    }
                } else {
                    // Still in progress; emit state 2 (input hashes)
                    Some(PendingMergeState::Inputs {
                        curr: handle.metadata.input_curr_hash,
                        snap: handle.metadata.input_snap_hash,
                    })
                }
            }
        }
    }

    /// Resolve any pending async or shared merge without committing it.
    ///
    /// This ensures that if this level has an async/shared merge in progress,
    /// we wait for it to complete and cache its result. This is necessary
    /// before cloning the bucket list, as unresolved merges would be
    /// lost during cloning.
    ///
    /// Merge failures are propagated as errors. A transient-IO (ENOSPC/EDQUOT)
    /// failure is recoverable and re-classified by the caller; genuine
    /// corruption stays fatal (#3478). The error is re-materialized as a
    /// *classified* `BucketError` (via the cached structured `MergeError`), so
    /// repeated calls keep surfacing the same classified error rather than
    /// silently returning `Ok` once cached.
    pub fn resolve_pending_merge(&mut self) -> Result<()> {
        match &mut self.next {
            Some(PendingMerge::Async(ref mut handle)) => {
                // Always call resolve(): if Pending it blocks and caches; if
                // already Ready(Err) it re-surfaces the cached classified
                // error (so a poisoned level is never silently treated as Ok).
                handle.resolve()?;
            }
            Some(PendingMerge::Shared(ref mut handle)) => {
                handle.resolve()?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Non-blocking pre-resolution of this level's pending merge.
    ///
    /// If `next` is an async/shared merge that has already completed in the
    /// background, cache its result (`Ready`) so the eventual boundary
    /// `commit()` takes the instant fast path rather than blocking the event
    /// loop for the remainder of a deep-level merge (#3867). No-op for
    /// `InMemory`, absent, or still-running merges. Never blocks, never commits,
    /// and never records into `completed_merges` — `commit()` remains the sole
    /// site that promotes `next → curr` and records the completed merge.
    pub fn poll_pending_merge(&mut self) {
        match &mut self.next {
            Some(PendingMerge::Async(handle)) => handle.try_resolve(),
            Some(PendingMerge::Shared(handle)) => handle.try_resolve(),
            _ => {}
        }
    }

    /// The `MergeKey` of this level's pending merge, if any. Used to clear the
    /// shared merge map when resetting a transient-IO-failed level (#3478).
    fn pending_merge_key(&self) -> Option<MergeKey> {
        match &self.next {
            Some(PendingMerge::Async(handle)) => Some(handle.merge_key.clone()),
            Some(PendingMerge::Shared(handle)) => Some(handle.metadata.merge_key.clone()),
            _ => None,
        }
    }

    /// Reset a level whose pending merge failed transiently, so the next close
    /// re-issues a fresh merge instead of being served a poisoned `Ready(Err)`
    /// (#3478, §3). Clears the pending merge and drops the in-flight guard
    /// (signalling any reattached consumers via the guard's `Drop`).
    fn reset_failed_merge(&mut self) {
        self.next = None;
        self.in_flight_guard = None;
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
                // For async, only return if we have a cached successful result
                if let MergeRecvState::Ready(Ok(ref bucket)) = handle.state {
                    Some(bucket.as_ref())
                } else {
                    None
                }
            }
            Some(PendingMerge::Shared(handle)) => {
                if let Some(Ok(ref bucket)) = handle.cached_result {
                    Some(bucket.as_ref())
                } else {
                    None
                }
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
            keep_dead_entries = ?ctx.keep_dead_entries,
            normalize_init = ?ctx.normalize_init,
            "prepare_with_normalization: about to merge"
        );

        // Unified merge dedup: check completed cache → in-flight → start new.
        // This replaces the old completed-cache-only check and adds in-flight dedup
        // (parity with stellar-core getMergeFuture/putMergeFuture).
        if let Some(merge_map) = ctx.merge_map {
            // Dedup uses P24+ waiver (MergeKey ignores normalize_init). Verify callers
            // only enter this path with Preserve (production) to catch test misuse early.
            debug_assert!(
                matches!(ctx.normalize_init, InitEntryPolicy::Preserve),
                "in-flight dedup requires Preserve normalize_init (P24+ waiver)"
            );

            let curr_hash = curr_for_merge.hash();
            let snap_hash = incoming.hash();
            let key = MergeKey::new(ctx.keep_dead_entries, curr_hash, snap_hash);

            let metadata = crate::merge_map::SharedMergeMetadata {
                merge_key: key.clone(),
                input_curr_hash: curr_hash,
                input_snap_hash: snap_hash,
                input_file_paths: {
                    let mut paths = Vec::new();
                    if let Some(path) = curr_for_merge.backing_file_path() {
                        paths.push(path.to_path_buf());
                    }
                    if let Some(path) = incoming.backing_file_path() {
                        paths.push(path.to_path_buf());
                    }
                    paths
                },
                level: self.level,
            };

            let bucket_dir_clone = ctx.bucket_dir.map(|p| p.to_path_buf());
            let slot = merge_map.write().unwrap().get_or_start(
                &key,
                metadata,
                Arc::clone(merge_map),
                |output_hash| {
                    // Load completed merge result from disk
                    if let Some(ref dir) = bucket_dir_clone {
                        let path = dir.join(canonical_bucket_filename(output_hash));
                        if path.exists() {
                            match Bucket::from_xdr_file_disk_backed(&path) {
                                Ok(bucket) if bucket.hash() == *output_hash => {
                                    return Some(Arc::new(bucket));
                                }
                                Ok(bucket) => {
                                    tracing::warn!(
                                        expected = %output_hash,
                                        actual = %bucket.hash(),
                                        "Merge-map cached result hash mismatch"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "Failed to load cached merge result"
                                    );
                                }
                            }
                        }
                    }
                    None
                },
            );

            match slot {
                crate::merge_map::MergeSlot::Completed(bucket) => {
                    if let Some(ref counters) = ctx.merge_counters {
                        counters.record_finished_reattachment();
                    }
                    tracing::debug!(
                        level = self.level,
                        output_hash = %bucket.hash(),
                        "Reusing completed merge result from merge map"
                    );
                    self.next = Some(PendingMerge::InMemory((*bucket).clone()));
                    return Ok(());
                }
                crate::merge_map::MergeSlot::InFlight { receiver, metadata } => {
                    if let Some(ref counters) = ctx.merge_counters {
                        counters.record_running_reattachment();
                    }
                    tracing::debug!(level = self.level, "Reattaching to in-flight merge (dedup)");
                    self.next = Some(PendingMerge::Shared(SharedMergeHandle::new(
                        receiver, metadata,
                    )));
                    return Ok(());
                }
                crate::merge_map::MergeSlot::New { guard } => {
                    // Start merge with guard for in-flight dedup tracking.
                    // The guard will signal completion when the merge finishes.
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
                            bucket_list_db: ctx.bucket_list_db,
                        });
                        // Store the guard alongside the handle so it signals on commit.
                        self.next = Some(PendingMerge::Async(handle));
                        // Store guard for later signaling when merge commits.
                        // We attach it to the handle's merge_key for lookup.
                        self.in_flight_guard = Some(guard);
                    } else {
                        // Level 0 sync merge
                        let merged = merge_buckets(
                            &curr_for_merge,
                            &incoming,
                            &MergeOptions {
                                keep_dead_entries: ctx.keep_dead_entries,
                                max_protocol_version: protocol_version,
                                normalize_init_entries: ctx.normalize_init,
                                shadow_buckets,
                                counters: ctx.merge_counters.as_deref(),
                                metadata_policy: MetadataPolicy::CurrentProtocol,
                            },
                        )?;
                        // Signal guard completion immediately for sync merge
                        guard.complete(Arc::new(merged.clone()));
                        self.next = Some(PendingMerge::InMemory(merged));
                    }
                    return Ok(());
                }
            }
        }

        // Fallback: no merge_map set → start merge without dedup (test paths)
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
                bucket_list_db: ctx.bucket_list_db,
            });
            self.next = Some(PendingMerge::Async(handle));
        } else {
            // Level 0 should use prepare_first_level, but if called here, do sync merge.
            let merged = merge_buckets(
                &curr_for_merge,
                &incoming,
                &MergeOptions {
                    keep_dead_entries: ctx.keep_dead_entries,
                    max_protocol_version: protocol_version,
                    normalize_init_entries: ctx.normalize_init,
                    shadow_buckets,
                    counters: ctx.merge_counters.as_deref(),
                    metadata_policy: MetadataPolicy::CurrentProtocol,
                },
            )?;
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
            // Level 0 always keeps tombstones and never normalizes INIT entries.
            // Use CurrentProtocol metadata policy to match stellar-core's
            // mergeInMemory level-0 behavior (LiveBucket.cpp:569).
            merge_buckets(
                &self.curr,
                &incoming,
                &MergeOptions {
                    keep_dead_entries: DeadEntryPolicy::Keep,
                    max_protocol_version: protocol_version,
                    normalize_init_entries: InitEntryPolicy::Preserve,
                    metadata_policy: MetadataPolicy::CurrentProtocol,
                    ..Default::default()
                },
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
        // Async/Shared variants would need to be resolved first, but since Clone
        // takes &self (not &mut self), we can't resolve them here.
        // Instead, we only clone the cached result if available.
        let cloned_next = match &self.next {
            None => None,
            Some(PendingMerge::InMemory(bucket)) => Some(PendingMerge::InMemory(bucket.clone())),
            Some(PendingMerge::Async(handle)) => {
                // If the async merge has completed successfully, clone it as InMemory.
                // Otherwise, skip the pending merge.
                if let MergeRecvState::Ready(Ok(ref result)) = handle.state {
                    Some(PendingMerge::InMemory((**result).clone()))
                } else {
                    // Async merge not yet resolved or failed - skip it
                    // The caller should resolve merges before cloning if they need them
                    tracing::warn!(
                        level = self.level,
                        "Cloning BucketLevel with unresolved async merge - merge will be lost"
                    );
                    None
                }
            }
            Some(PendingMerge::Shared(handle)) => {
                // If the shared merge has resolved, clone as InMemory.
                // Otherwise, skip (same as Async).
                if let Some(Ok(ref bucket)) = handle.cached_result {
                    Some(PendingMerge::InMemory((**bucket).clone()))
                } else {
                    tracing::warn!(
                        level = self.level,
                        "Cloning BucketLevel with unresolved shared merge - merge will be lost"
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
            in_flight_guard: None, // Guards are not cloneable; cloned levels don't own merges
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
    /// Counters for the live eviction scan (shared across all scan calls and
    /// with derived `BucketListSnapshot`s, so the background scan records into
    /// the same instance the app reads). All-atomic; cheaply `Arc`-clonable.
    eviction_counters: Arc<EvictionCounters>,
    /// Background thread handle for async bucket persistence.
    /// Bounded to one concurrent write; the next add_batch waits for completion.
    ///
    /// The error payload is a structured [`MergeError`] (not a bare `String`)
    /// so a transient ENOSPC during persist stays distinguishable from genuine
    /// corruption at the flush site (#3478).
    pending_persist:
        Option<std::thread::JoinHandle<std::result::Result<(), crate::merge_map::MergeError>>>,
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
    keep_dead: DeadEntryPolicy,
    protocol_version: u32,
    config: &BucketListDbConfig,
) -> Result<Bucket> {
    if let Some(dir) = bucket_dir {
        let temp_path = temp_merge_path(dir);
        let (hash, entry_count) = merge_buckets_to_file(
            input_curr,
            input_snap,
            &temp_path,
            &MergeOptions {
                keep_dead_entries: keep_dead,
                max_protocol_version: protocol_version,
                normalize_init_entries: InitEntryPolicy::Preserve,
                ..Default::default()
            },
        )?;
        if entry_count == 0 {
            let _ = std::fs::remove_file(&temp_path);
            Ok(Bucket::empty())
        } else {
            promote_temp_to_canonical(&temp_path, dir, &hash, "perform_merge", config)
        }
    } else {
        merge_buckets(
            input_curr,
            input_snap,
            &MergeOptions {
                keep_dead_entries: keep_dead,
                max_protocol_version: protocol_version,
                normalize_init_entries: InitEntryPolicy::Preserve,
                ..Default::default()
            },
        )
    }
}

/// Load a bucket by hash via `FnMut` closure and verify the returned hash matches.
fn load_and_verify<F>(hash: &Hash256, load_bucket: &mut F) -> Result<Bucket>
where
    F: FnMut(&Hash256) -> Result<Bucket>,
{
    let bucket = load_bucket(hash)?;
    if bucket.hash() != *hash {
        return Err(BucketError::HashMismatch {
            expected: hash.to_hex(),
            actual: bucket.hash().to_hex(),
        });
    }
    Ok(bucket)
}

/// Load a bucket by hash via shared `Fn` closure and verify the returned hash matches.
fn load_and_verify_shared<F>(hash: &Hash256, load_bucket: &F) -> Result<Bucket>
where
    F: Fn(&Hash256) -> Result<Bucket>,
{
    let bucket = load_bucket(hash)?;
    if bucket.hash() != *hash {
        return Err(BucketError::HashMismatch {
            expected: hash.to_hex(),
            actual: bucket.hash().to_hex(),
        });
    }
    Ok(bucket)
}

/// Load a bucket by hash, short-circuiting recognized empty-bucket sentinels.
///
/// Returns the canonical sentinel bucket (zero-hash empty or `empty_hash()`-hash
/// empty) when applicable; otherwise calls [`load_and_verify`]. Use this in
/// restore and merge-restart paths where the loader closure may not itself be
/// sentinel-aware. This mirrors `BucketManager::load_bucket`, which short-circuits
/// the same sentinels via `Bucket::for_sentinel_hash`.
fn load_or_sentinel<F>(hash: &Hash256, load_bucket: &mut F) -> Result<Bucket>
where
    F: FnMut(&Hash256) -> Result<Bucket>,
{
    if let Some(b) = Bucket::for_sentinel_hash(hash) {
        return Ok(b);
    }
    load_and_verify(hash, load_bucket)
}

/// Shared-closure variant of [`load_or_sentinel`] for use under
/// `std::thread::scope`.
fn load_or_sentinel_shared<F>(hash: &Hash256, load_bucket: &F) -> Result<Bucket>
where
    F: Fn(&Hash256) -> Result<Bucket>,
{
    if let Some(b) = Bucket::for_sentinel_hash(hash) {
        return Ok(b);
    }
    load_and_verify_shared(hash, load_bucket)
}

/// Persist a batch of buckets to disk, returning a structured [`MergeError`]
/// that preserves the IO error classification (#3478).
///
/// Runs on the background persist thread. If multiple buckets fail, the
/// returned error preserves the most-severe class so corruption is never
/// masked by a co-occurring transient failure: `Corruption` wins over
/// `TransientIo`, which wins over `Other`. The message aggregates all failures.
fn persist_buckets_collecting_error(
    buckets_to_persist: Vec<(Arc<Bucket>, std::path::PathBuf)>,
) -> std::result::Result<(), crate::merge_map::MergeError> {
    let mut messages = Vec::new();
    // Track the most-severe class seen across failures.
    let mut worst: Option<crate::error::BucketErrorClass> = None;
    for (bucket, path) in buckets_to_persist {
        if let Err(e) = bucket.save_to_xdr_file(&path) {
            messages.push(format!("bucket {}: {}", bucket.hash().to_hex(), e));
            let class = e.error_class();
            worst = Some(match worst {
                None => class,
                Some(prev) => more_severe_class(prev, class),
            });
        }
    }
    if messages.is_empty() {
        Ok(())
    } else {
        Err(crate::merge_map::MergeError {
            class: worst.unwrap_or(crate::error::BucketErrorClass::Other),
            message: std::sync::Arc::from(messages.join("; ")),
        })
    }
}

/// Pick the more-severe of two error classes so corruption is never masked by
/// a co-occurring transient failure (#3478): `Corruption` > `TransientIo` >
/// `Other`. (`Other` is fatal too, but a transient class is the more specific
/// recoverable signal, so it ranks above `Other` for the recovery decision.)
fn more_severe_class(
    a: crate::error::BucketErrorClass,
    b: crate::error::BucketErrorClass,
) -> crate::error::BucketErrorClass {
    use crate::error::BucketErrorClass::*;
    match (a, b) {
        (Corruption, _) | (_, Corruption) => Corruption,
        (TransientIo(e), _) | (_, TransientIo(e)) => TransientIo(e),
        _ => Other,
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
            eviction_counters: Arc::new(EvictionCounters::new()),
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

    /// Wait for any pending background persist to complete and propagate errors.
    /// Call before writing state that references bucket files (HAS, LCL, publish).
    pub fn flush_pending_persist(&mut self) -> Result<()> {
        if let Some(handle) = self.pending_persist.take() {
            let result: std::result::Result<(), crate::merge_map::MergeError> = handle
                .join()
                .expect("background bucket persist thread panicked");
            // Re-materialize a classified BucketError (preserving the original
            // ErrorKind/errno) instead of flattening to ErrorKind::Other, so
            // the persist flush site can distinguish a transient ENOSPC from
            // corruption (#3478).
            result.map_err(|e| e.to_bucket_error())?;
        }
        Ok(())
    }

    /// Take the pending persist join handle without joining it.
    ///
    /// The caller is responsible for joining the handle. This allows
    /// releasing the bucket list lock before the (potentially slow)
    /// thread join, preventing lock contention with concurrent readers.
    pub fn take_pending_persist(
        &mut self,
    ) -> Option<std::thread::JoinHandle<std::result::Result<(), crate::merge_map::MergeError>>>
    {
        self.pending_persist.take()
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

    /// Returns a reference to the eviction counters.
    ///
    /// These are shared (via `Arc`) with any `BucketListSnapshot` derived from
    /// this list, so a background eviction scan running on a snapshot records
    /// into the same instance an app reads here.
    pub fn eviction_counters(&self) -> &EvictionCounters {
        &self.eviction_counters
    }

    /// Returns the shared `Arc` to the eviction counters.
    ///
    /// Used when constructing a `BucketListSnapshot` so its background scan
    /// records into the same instance as this list.
    pub(crate) fn eviction_counters_arc(&self) -> Arc<EvictionCounters> {
        Arc::clone(&self.eviction_counters)
    }

    /// Resolve all pending async merges without committing them.
    ///
    /// This should be called before cloning a bucket list to ensure that all
    /// async merges are resolved and their results are cached, preventing data
    /// loss during cloning.
    ///
    /// # Recovery semantics (#3478)
    ///
    /// On a **transient-IO** (ENOSPC/EDQUOT) merge failure — environmental,
    /// no partial state committed — the failed level is RESET (its pending
    /// merge and shared-merge-map entry are cleared) so the next close
    /// re-issues a fresh merge once disk recovers, rather than being served a
    /// poisoned cached `Ready(Err)`. The error is still propagated so callers
    /// can back off (skip GC / abort a catchup attempt) without panicking.
    ///
    /// Genuine **corruption** (and any other class) is left terminal and
    /// propagated unchanged — we must NOT silently retry corruption.
    pub fn resolve_all_pending_merges(&mut self) -> Result<()> {
        for idx in 0..self.levels.len() {
            let result = self.levels[idx].resolve_pending_merge();
            if let Err(ref err) = result {
                if err.is_transient_io() {
                    // Recoverable: reset this level so the next close re-issues
                    // the merge, and clear the shared merge-map entry for its
                    // key so the re-issue starts fresh (not a stale in-flight /
                    // completed lookup).
                    let key = self.levels[idx].pending_merge_key();
                    self.levels[idx].reset_failed_merge();
                    if let (Some(key), Some(merge_map)) = (key, self.merge_map.as_ref()) {
                        if let Ok(mut map) = merge_map.write() {
                            map.in_flight.remove(&key);
                            map.remove_merge(&key);
                        }
                    }
                    tracing::warn!(
                        level = idx,
                        error = %err,
                        "Transient IO failure resolving bucket merge; level reset to re-merge \
                         on next close (recoverable — not corruption)"
                    );
                }
                return result;
            }
        }
        Ok(())
    }

    /// Non-blocking sweep that pre-resolves any completed background merges
    /// across all levels, so a later boundary `commit()` doesn't park the event
    /// loop on a deep-level merge that has already finished (#3867).
    ///
    /// A level-≥6 merge is *prepared* thousands of ledgers before its commit
    /// boundary, so it finishes in the background long beforehand; only its
    /// *observation* was deferred to the blocking `resolve()` under the held
    /// `RwLock<BucketList>` write lock. Calling this at the top of every
    /// `add_batch` observes such merges eagerly and non-blockingly (microseconds).
    ///
    /// Unlike [`resolve_all_pending_merges`](Self::resolve_all_pending_merges),
    /// this never blocks and never fails: a still-running merge is left pending,
    /// and any classified failure stays cached on the handle to be surfaced by
    /// the eventual blocking `resolve()` at commit. It touches only the `next`
    /// pending-merge field and never `curr`/`snap`, so it cannot change any
    /// consensus/interop hash.
    pub fn poll_pending_merges(&mut self) {
        for level in &mut self.levels {
            level.poll_pending_merge();
        }
    }

    /// Get the hash of the entire BucketList.
    ///
    /// This is computed by hashing all level hashes together.
    pub fn hash(&self) -> Hash256 {
        let mut hasher = Sha256::new();

        for level in &self.levels {
            hasher.update(level.hash().as_bytes());
        }

        Hash256::from_sha256(hasher)
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
        let total_account_bytes = counters.size_for_type(stellar_xdr::LedgerEntryType::Account);
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
        entry_type: stellar_xdr::LedgerEntryType,
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
        entry_types: &[stellar_xdr::LedgerEntryType],
        mut callback: F,
    ) -> Result<bool>
    where
        F: FnMut(&BucketEntry) -> bool,
    {
        use stellar_xdr::LedgerKey;

        let type_set: HashSet<stellar_xdr::LedgerEntryType> = entry_types.iter().copied().collect();
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
    /// Test-only materializing oracle: it collects every live entry into memory
    /// (~52 GB at mainnet scale), so production code uses the streaming
    /// [`live_entries_iter()`](Self::live_entries_iter) instead. This helper is
    /// retained solely as an independent cross-check for the iterator in
    /// `live_iterator::tests::test_matches_live_entries`.
    #[cfg(test)]
    pub(crate) fn live_entries(&self) -> Result<Vec<LedgerEntry>> {
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
        })?;
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

        // #3867: eagerly, non-blockingly observe any background merge that has
        // already finished, so the spill/commit loop below takes the instant
        // `Ready` fast path instead of parking the held write lock (and every
        // event-loop bucket-list reader) for the rest of a deep-level merge.
        // This is a microsecond sweep; the genuinely-unavoidable case (a merge
        // slower than its whole window) still blocks at commit, matching
        // stellar-core.
        self.poll_pending_merges();

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
                let normalize_init = InitEntryPolicy::Preserve; // Never normalize INIT to LIVE during merges
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
                        bucket_list_db: self.bucket_list_db_config.clone().unwrap_or_default(),
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
                let result: std::result::Result<(), crate::merge_map::MergeError> = handle
                    .join()
                    .expect("background bucket persist thread panicked");
                // Preserve the structured class/errno (#3478).
                result.map_err(|e| e.to_bucket_error())?;
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
                    persist_buckets_collecting_error(buckets_to_persist)
                }));
            }
        }

        // Diagnostic (#3552): emit per-level curr/snap hashes for this ledger.
        // This is a read-only hook over `level_hashes()` + `hash()`; it
        // perturbs no bucket bytes and is a complete no-op unless the
        // `BUCKET_LIST_HASH_TRACE_TARGET` tracing target is enabled. It exists
        // so a live SSC v27 mission re-run can pinpoint the divergent level for
        // the L16 `bucketListHash` divergence (root-cause fix tracked in #3553).
        self.emit_per_level_hash_trace(ledger_seq);

        Ok(())
    }

    /// Diagnostic tracing target for the per-level `bucketListHash` hook (#3552).
    ///
    /// Enable with e.g. `RUST_LOG=bucket_list_hash=debug` (or via a subscriber
    /// filter) to log, after every `add_batch`, each level's `curr`/`snap`
    /// hashes plus the top-level `bucket_list_hash`. When the target is not
    /// enabled the hook does no work and reads no bucket state, so it cannot
    /// affect any hashed bytes (see `test_per_level_hash_trace_is_non_perturbing`).
    pub const BUCKET_LIST_HASH_TRACE_TARGET: &'static str = "bucket_list_hash";

    /// Read-only per-level hash hook (#3552). Logs each level's curr/snap hash
    /// and the overall `bucket_list_hash` for `ledger_seq` under the
    /// [`Self::BUCKET_LIST_HASH_TRACE_TARGET`] tracing target.
    ///
    /// Guarded by `tracing::enabled!` so it short-circuits to a no-op — reading
    /// nothing and allocating nothing — when the target is disabled. It only
    /// ever *reads* `level_hashes()`/`hash()`; it never mutates the bucket list,
    /// so it cannot perturb any bucket bytes or the computed hash regardless of
    /// whether it is enabled.
    fn emit_per_level_hash_trace(&self, ledger_seq: u32) {
        if !tracing::enabled!(
            target: BucketList::BUCKET_LIST_HASH_TRACE_TARGET,
            tracing::Level::DEBUG
        ) {
            return;
        }
        for (idx, level_hash, curr_hash, snap_hash) in self.level_hashes() {
            tracing::debug!(
                target: BucketList::BUCKET_LIST_HASH_TRACE_TARGET,
                ledger = ledger_seq,
                level = idx,
                level_hash = %level_hash,
                curr = %curr_hash,
                snap = %snap_hash,
                "per-level bucket hash",
            );
        }
        tracing::debug!(
            target: BucketList::BUCKET_LIST_HASH_TRACE_TARGET,
            ledger = ledger_seq,
            bucket_list_hash = %self.hash(),
            "bucket list hash",
        );
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

    fn keep_tombstone_entries(level: usize) -> DeadEntryPolicy {
        if bl_keep_tombstone_entries(level, BUCKET_LIST_LEVELS) {
            DeadEntryPolicy::Keep
        } else {
            DeadEntryPolicy::Remove
        }
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
                        if let MergeRecvState::Ready(Ok(ref result)) = handle.state {
                            hashes.push(result.hash());
                        }
                    }
                    PendingMerge::Shared(handle) => {
                        hashes.push(handle.metadata.input_curr_hash);
                        hashes.push(handle.metadata.input_snap_hash);
                        if let Some(Ok(ref bucket)) = handle.cached_result {
                            hashes.push(bucket.hash());
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
                        if let MergeRecvState::Ready(Ok(ref result)) = handle.state {
                            if let Some(path) = result.backing_file_path() {
                                paths.insert(path.to_path_buf());
                            }
                        }
                    }
                    PendingMerge::Shared(handle) => {
                        // Add input bucket files referenced by shared merge
                        for path in &handle.metadata.input_file_paths {
                            paths.insert(path.clone());
                        }
                        if let Some(Ok(ref bucket)) = handle.cached_result {
                            if let Some(path) = bucket.backing_file_path() {
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

        // Use default next states (all clear, no pending merges)
        let next_states = vec![None; BUCKET_LIST_LEVELS];

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
    /// * `next_states` - Vec of pending merge states for each level
    /// * `load_bucket` - Thread-safe function to load a bucket from its hash
    /// * `fan_out` - Optional cap on the number of workers that may *materialize*
    ///   a bucket (`load_bucket`) concurrently. `None` (or `Some(0)`) is the
    ///   default: unbounded, byte-for-byte the historical behavior — all
    ///   workers materialize at once. `Some(k)` caps concurrent materialization
    ///   to `k` via a counting semaphore ([`FanOutLimiter`]) acquired *around
    ///   the `load_bucket` calls*. The permit bounds the transient
    ///   per-load working set (the cold-catchup spike, #3235), NOT the resident
    ///   base — assembled buckets stay live regardless of `k`. The restored
    ///   `BucketList` and its `hash()` are identical for every `fan_out` value:
    ///   assembly indexes results by level, independent of scheduling order.
    pub fn restore_from_has_parallel<F>(
        hashes: &[(Hash256, Hash256)],
        next_states: &[Option<PendingMergeState>],
        load_bucket: F,
        fan_out: Option<usize>,
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
        if next_states.len() != BUCKET_LIST_LEVELS {
            return Err(BucketError::Serialization(format!(
                "Expected {} next states, got {}",
                BUCKET_LIST_LEVELS,
                next_states.len()
            )));
        }

        let load_bucket = &load_bucket;

        // Bound concurrent bucket materialization (the cold-catchup spike, #3245).
        // Unbounded by default (`None`/`Some(0)`) → no-op, current behavior.
        let limiter = crate::FanOutLimiter::from_cap(fan_out);
        let limiter = &limiter;

        // Collect (level_index, output_hash) for levels with completed merge outputs.
        // Skip zero-hash outputs as a belt-and-suspenders defense; parse_next_states()
        // canonicalizes these to None, but direct callers might not.
        let output_hashes: Vec<(usize, Hash256)> = next_states
            .iter()
            .enumerate()
            .filter_map(|(i, state)| match state {
                Some(PendingMergeState::Output(h)) if !h.is_zero() => Some((i, *h)),
                _ => None,
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

                        // Acquire a permit around materialization so at most
                        // `fan_out` workers build buckets at once (no-op when
                        // unbounded). Held across both loads (the level
                        // worker's transient working set); released on drop
                        // after the buckets are built and returned.
                        let _permit = limiter.acquire();
                        let curr = load_or_sentinel_shared(curr_hash, load_bucket)?;
                        let snap = load_or_sentinel_shared(snap_hash, load_bucket)?;

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
                            // Same permit gate as the level workers — the
                            // output bucket (e.g. level 10's ~47M-entry merge
                            // result) is a heavy materialization.
                            let _permit = limiter.acquire();
                            let bucket = load_and_verify_shared(&hash, load_bucket)?;
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
            eviction_counters: Arc::new(EvictionCounters::new()),
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
    /// * `next_states` - Pending merge state for each level
    /// * `load_bucket` - Function to load a bucket from its hash
    pub fn restore_from_has<F>(
        hashes: &[(Hash256, Hash256)],
        next_states: &[Option<PendingMergeState>],
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
        if next_states.len() != BUCKET_LIST_LEVELS {
            return Err(BucketError::Serialization(format!(
                "Expected {} next states, got {}",
                BUCKET_LIST_LEVELS,
                next_states.len()
            )));
        }

        let mut levels = Vec::with_capacity(BUCKET_LIST_LEVELS);

        for (i, (curr_hash, snap_hash)) in hashes.iter().enumerate() {
            let level_start = std::time::Instant::now();
            let curr = load_or_sentinel(curr_hash, &mut load_bucket)?;
            let snap = load_or_sentinel(snap_hash, &mut load_bucket)?;

            tracing::info!(
                level = i,
                curr_entries = curr.len(),
                snap_entries = snap.len(),
                elapsed_ms = level_start.elapsed().as_millis() as u64,
                "restore_from_has: loaded level"
            );

            // Load completed merge output if present.
            // Inputs-state merges are handled later in restart_merges_from_has.
            // Skip zero-hash outputs as a defensive guard.
            let next: Option<PendingMerge> = match &next_states[i] {
                Some(PendingMergeState::Output(output_hash)) if !output_hash.is_zero() => {
                    tracing::debug!(
                        level = i,
                        output_hash = %output_hash.to_hex(),
                        "restore_from_has: loading completed merge output"
                    );
                    Some(PendingMerge::InMemory(load_and_verify(
                        output_hash,
                        &mut load_bucket,
                    )?))
                }
                _ => None,
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
            eviction_counters: Arc::new(EvictionCounters::new()),
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
        next_states: &[Option<PendingMergeState>],
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

        if next_states.len() != BUCKET_LIST_LEVELS {
            return Err(BucketError::Serialization(format!(
                "Expected {} next states, got {}",
                BUCKET_LIST_LEVELS,
                next_states.len()
            )));
        }

        // RESTART_DIAG: log per-level HAS state and bucket list state at entry
        for (i, state) in next_states.iter().enumerate() {
            let state_desc = match state {
                None => "clear".to_string(),
                Some(PendingMergeState::Output(h)) => format!("output:{}", h.to_hex()),
                Some(PendingMergeState::Inputs { curr, snap }) => {
                    format!("inputs:curr={},snap={}", curr.to_hex(), snap.to_hex())
                }
            };
            let has_next = self.levels[i].next.is_some();
            tracing::warn!(
                level = i,
                ledger = ledger,
                has_state = %state_desc,
                has_existing_next = has_next,
                curr_hash = %self.levels[i].curr.hash().to_hex(),
                snap_hash = %self.levels[i].snap.hash().to_hex(),
                "RESTART_DIAG: live bucket list level state at restart entry"
            );
        }

        // Phase 1: Collect work items (sequential, fast — just loads input buckets)
        struct MergeWorkItem {
            level: usize,
            input_curr: Bucket,
            input_snap: Bucket,
            keep_dead: DeadEntryPolicy,
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

            let state = &next_states[i];
            if let Some(PendingMergeState::Inputs {
                curr: ref curr_hash,
                snap: ref snap_hash,
            }) = state
            {
                let input_curr = load_or_sentinel(curr_hash, &mut load_bucket)?;
                let input_snap = load_or_sentinel(snap_hash, &mut load_bucket)?;

                tracing::warn!(
                    level = i,
                    ledger = ledger,
                    source = "has_state2",
                    input_curr_hash = %curr_hash.to_hex(),
                    input_snap_hash = %snap_hash.to_hex(),
                    keep_dead = ?Self::keep_tombstone_entries(i),
                    protocol_version = protocol_version,
                    "RESTART_DIAG: queueing merge from HAS input hashes"
                );

                work_items.push(MergeWorkItem {
                    level: i,
                    input_curr,
                    input_snap,
                    keep_dead: Self::keep_tombstone_entries(i),
                });
            }
        }

        // Phase 2: Spawn all merges in parallel via spawn_blocking
        if !work_items.is_empty() {
            let bucket_dir = self.bucket_dir.clone();
            let bucket_list_db = self.bucket_list_db_config.clone().unwrap_or_default();

            let handles_with_levels: Vec<_> = work_items
                .into_iter()
                .map(|work| {
                    let bucket_dir = bucket_dir.clone();
                    let bucket_list_db = bucket_list_db.clone();
                    let level = work.level;
                    let handle = tokio::task::spawn_blocking(move || {
                        let start = std::time::Instant::now();
                        let level = work.level;

                        let result = perform_merge(
                            &work.input_curr,
                            &work.input_snap,
                            bucket_dir.as_ref(),
                            work.keep_dead,
                            protocol_version,
                            &bucket_list_db,
                        );

                        let elapsed = start.elapsed();
                        match &result {
                            Ok(bucket) => {
                                tracing::warn!(
                                    level,
                                    duration_ms = elapsed.as_millis() as u64,
                                    merged_hash = %bucket.hash().to_hex(),
                                    merged_entries = bucket.len(),
                                    "RESTART_DIAG: HAS state=2 merge completed"
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    level,
                                    duration_ms = elapsed.as_millis() as u64,
                                    error = %e,
                                    "RESTART_DIAG: HAS state=2 merge failed"
                                );
                            }
                        }

                        result.map(|bucket| (level, bucket))
                    });
                    (level, handle)
                })
                .collect();

            // Phase 3: Await all and install results.
            // Tasks are already running in parallel on the blocking pool;
            // sequential await just processes results in order.
            for (level, handle) in handles_with_levels {
                let ctx = format!("restart-merge-level-{level}");
                let (lvl, merged) = henyey_common::await_blocking_logged(&ctx, handle)
                    .await
                    .map_err(|e| {
                        BucketError::Merge(format!("merge task failed (level {level}): {e}"))
                    })??;
                self.levels[lvl].next = Some(PendingMerge::InMemory(merged));
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
            let merge_protocol_version = prev_snap.protocol_version()?.unwrap_or(protocol_version);
            // Note: stellar-core never normalizes INIT to LIVE during merges - the keepTombstoneEntries
            // flag only affects DEAD entry filtering, not INIT entry transformation.
            let keep_dead = Self::keep_tombstone_entries(i);
            let normalize_init = InitEntryPolicy::Preserve; // stellar-core never normalizes INIT to LIVE during merges
            let use_empty_curr = Self::should_merge_with_empty_curr(merge_start_ledger, i);

            // RESTART_DIAG: log all merge parameters for post-mortem comparison
            // with steady-state add_batch_internal parameters.
            tracing::warn!(
                level = i,
                ledger = ledger,
                source = "structure_based",
                merge_start_ledger = merge_start_ledger,
                use_empty_curr = use_empty_curr,
                keep_dead = ?keep_dead,
                merge_protocol_version = merge_protocol_version,
                caller_protocol_version = protocol_version,
                level_curr_hash = %self.levels[i].curr.hash().to_hex(),
                level_snap_hash = %self.levels[i].snap.hash().to_hex(),
                prev_snap_hash = %prev_snap.hash().to_hex(),
                "RESTART_DIAG: starting structure-based merge"
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
                    bucket_list_db: self.bucket_list_db_config.clone().unwrap_or_default(),
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
    /// BUCKETLISTDB_SPEC §12.3: Temporary entries are deleted; persistent entries
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
    ///
    /// `ledger_version` is the protocol version the scan is based on; it is
    /// captured on the result so a stale background scan can be invalidated at
    /// resolve time via [`EvictionResult::is_valid`] (BUCKETLISTDB §12.5/§12.6).
    pub fn scan_for_eviction_incremental(
        &self,
        mut iter: EvictionIterator,
        current_ledger: u32,
        ledger_version: u32,
        settings: &StateArchivalSettings,
    ) -> Result<EvictionResult> {
        let mut result = EvictionResult {
            candidates: Vec::new(),
            end_iterator: iter.clone(),
            bytes_scanned: 0,
            scan_complete: false,
            // Capture the network state this scan is based on so a stale
            // background scan can be invalidated at resolve time
            // (EvictionResult::is_valid). Spec: BUCKETLISTDB §12.5/§12.6.
            initial_ledger_seq: current_ledger,
            initial_ledger_version: ledger_version,
            initial_archival_settings: settings.clone(),
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
        let mut seen_keys: HashSet<LedgerKey> = HashSet::new();

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

            // Per-bucket "stuck" check before scanning: if the bucket is too
            // large to fully scan within its update period, record an
            // incomplete scan. Parity: stellar-core checks this at the top of
            // each loop iteration (LiveBucketList.cpp:158-173, BucketListSnapshot.cpp:617).
            if eviction_scan_is_stuck(&iter, settings.eviction_scan_size, bucket.byte_size()) {
                self.eviction_counters.record_incomplete_scan();
            }

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
                let wrapped = iter.advance_to_next_bucket(settings.starting_eviction_scan_level);

                // On a full-list wrap (return to the configured starting level
                // after the top level), close the eviction cycle and publish
                // the period + average-age gauges. Parity: stellar-core's
                // updateEvictionIterAndRecordStats calls
                // submitMetricsAndRestartCycle exactly on the kNumLevels wrap
                // (LiveBucketList.cpp:138-143).
                if wrapped {
                    self.eviction_counters
                        .submit_metrics_and_restart_cycle(current_ledger);
                }

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

        // Record total bytes scanned this ledger into the shared counters.
        // Parity note: stellar-core declares `bytesScannedForEviction` but never
        // increments it (dead there); henyey populating it is additive/benign.
        self.eviction_counters
            .record_bytes_scanned(result.bytes_scanned);

        result.end_iterator = iter;

        Ok(result)
    }

    /// Scan a region of a bucket for evictable entries (scan phase only).
    ///
    /// Returns (entries_scanned, bytes_used, finished_bucket).
    ///
    /// Delegates to [`crate::eviction::scan_bucket_region`] with `BucketList::get` lookups.
    fn scan_bucket_region(
        &self,
        bucket: &Bucket,
        iter: &mut EvictionIterator,
        max_bytes: u64,
        current_ledger: u32,
        candidates: &mut Vec<EvictionCandidate>,
        seen_keys: &mut HashSet<LedgerKey>,
    ) -> Result<(usize, u64, bool)> {
        crate::eviction::scan_bucket_region(
            bucket,
            iter,
            max_bytes,
            current_ledger,
            candidates,
            seen_keys,
            |key| self.get(key),
        )
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
            let result: std::result::Result<(), crate::merge_map::MergeError> = match handle.join()
            {
                Ok(r) => r,
                Err(_) => return,
            };
            if let Err(e) = result {
                tracing::warn!(error = %e, "Background bucket persist failed during drop");
            }
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
            eviction_counters: self.eviction_counters.clone(),
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
    use crate::merge::{merge_buckets, MergeOptions};
    use stellar_xdr::*;

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

    /// Drive a fixed 16-ledger `add_batch` sequence (a genesis INIT batch at L1,
    /// loadgen-shaped INIT batches at L11–L14, empty elsewhere) and return the
    /// per-ledger `bucket_list_hash` chain.
    ///
    /// Used to prove the #3552 per-level instrumentation hook is non-perturbing:
    /// the chain must be identical whether or not the tracing target is enabled.
    fn run_sixteen_ledger_chain() -> Vec<Hash256> {
        const LOADGEN_COUNTS: [(u32, u32); 4] = [(11, 22), (12, 24), (13, 30), (14, 24)];
        let mut bl = BucketList::new();
        let mut seed: u32 = 1;
        let mut chain = Vec::new();
        for ledger in 1..=16u32 {
            let mut init = Vec::new();
            if ledger == 1 {
                for _ in 0..20 {
                    let mut b = [0u8; 32];
                    b[..4].copy_from_slice(&seed.to_be_bytes());
                    init.push(make_account_entry(b, 100_000_000));
                    seed += 1;
                }
            }
            if let Some(&(_, count)) = LOADGEN_COUNTS.iter().find(|(l, _)| *l == ledger) {
                for _ in 0..count {
                    let mut b = [0u8; 32];
                    b[..4].copy_from_slice(&seed.to_be_bytes());
                    init.push(make_account_entry(b, 100_000_000));
                    seed += 1;
                }
            }
            bl.add_batch(
                ledger,
                27,
                BucketListType::Live,
                init,
                Vec::new(),
                Vec::new(),
            )
            .expect("add_batch");
            chain.push(bl.hash());
        }
        chain
    }

    /// #3552: the per-level `bucketListHash` instrumentation hook must be
    /// provably non-perturbing — enabling or disabling the tracing target must
    /// not change a single hashed byte of the bucket list.
    ///
    /// We run the identical 16-ledger `add_batch` sequence twice: once with the
    /// `bucket_list_hash` target disabled (the default in tests) and once with a
    /// subscriber that enables it at DEBUG. The full per-ledger hash chains must
    /// be byte-identical, proving the hook reads (and emits) only and never
    /// touches bucket state.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_per_level_hash_trace_is_non_perturbing() {
        use henyey_common::test_support::tracing_capture::capture_events;

        // Hook disabled (no subscriber enables the target).
        let chain_off = run_sixteen_ledger_chain();

        // Hook enabled: capture every event emitted under the diagnostic target
        // for the duration of this run. `capture_events` installs a
        // `Registry`-backed subscriber (which records the `bucket_list_hash`
        // target's DEBUG events) and serializes via a process-wide mutex so it
        // does not interfere with other parallel tracing-capture tests.
        let mut chain_on = Vec::new();
        let events = capture_events(|| {
            chain_on = run_sixteen_ledger_chain();
        });

        // The CORE property: byte-identical per-ledger hash chain with the hook
        // on vs off. This proves the hook is non-perturbing — it only reads and
        // emits, never mutating bucket state. This assertion is fully
        // deterministic and is the real guarantee #3552 needs.
        assert_eq!(
            chain_off, chain_on,
            "per-level hash trace hook must not change any bucket_list_hash"
        );

        // Best-effort emission check. `emit_per_level_hash_trace` gates on
        // `tracing::enabled!`, whose per-callsite `Interest` is cached in a
        // *global* (process-wide) cache in tracing-core. Under parallel
        // `cargo test`, that cache can be poisoned to `never` by the hook's
        // callsite being first evaluated under a non-interested dispatcher
        // (e.g. the `chain_off` run above, or a concurrent test), in which case
        // `enabled!` short-circuits and our thread-local subscriber records
        // nothing — a known, environmental tracing-capture race (#3552/#3554,
        // and the sibling verify::tests flake). We therefore tolerate a 0-event
        // capture but, when the hook *does* fire, assert the counts are exactly
        // right (11 levels × 16 ledgers per-level events, 16 list events) — this
        // still catches any real over/under-emission bug deterministically.
        let level_records = events
            .iter()
            .filter(|e| e.message.contains("per-level bucket hash"))
            .count();
        let list_records = events
            .iter()
            .filter(|e| e.message.contains("bucket list hash"))
            .count();
        if level_records != 0 || list_records != 0 {
            assert_eq!(
                level_records,
                BUCKET_LIST_LEVELS * 16,
                "expected one per-level record per level per ledger when the hook fires"
            );
            assert_eq!(
                list_records, 16,
                "expected one bucket_list_hash record per ledger when the hook fires"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_snapshot_lookup_after_genesis_add_batch() {
        // Regression test for #2357: BucketListSnapshot::get_result should find
        // entries added via add_batch (the genesis path).
        let mut bl = BucketList::new();
        let entry = make_account_entry([1u8; 32], 100);
        bl.add_batch(
            1,
            0, // genesis protocol_version = 0
            BucketListType::Live,
            vec![entry],
            vec![],
            vec![],
        )
        .unwrap();

        // Direct lookup should work
        let key = make_account_key([1u8; 32]);
        assert!(
            bl.get(&key).unwrap().is_some(),
            "Direct BucketList::get must find the entry"
        );

        // Snapshot lookup must also work
        let header = LedgerHeader {
            ledger_seq: 1,
            ..Default::default()
        };
        let snapshot = crate::BucketListSnapshot::new(&bl, header);
        let found = snapshot.get_result(&key).unwrap();
        assert!(
            found.is_some(),
            "BucketListSnapshot::get_result must find the entry added via add_batch"
        );
        if let LedgerEntryData::Account(acct) = found.unwrap().data {
            assert_eq!(acct.balance, 100);
        } else {
            panic!("Expected Account entry");
        }
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
                    keep_dead_entries: DeadEntryPolicy::Remove,
                    normalize_init: InitEntryPolicy::NormalizeToLive,
                    use_empty_curr: false,
                    bucket_dir: None,
                    merge_map: None,
                    merge_counters: None,
                    bucket_list_db: BucketListDbConfig::default(),
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
        let merged = merge_buckets(
            &Bucket::empty(),
            &bucket,
            &MergeOptions {
                keep_dead_entries: DeadEntryPolicy::Remove,
                max_protocol_version: TEST_PROTOCOL,
                normalize_init_entries: InitEntryPolicy::NormalizeToLive,
                ..Default::default()
            },
        )
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
            keep_dead_entries: DeadEntryPolicy::Remove,
            protocol_version: TEST_PROTOCOL,
            normalize_init: InitEntryPolicy::NormalizeToLive,
            shadow_buckets: vec![],
            level: 1,
            bucket_dir: Some(temp_dir.path().to_path_buf()),
            counters: None,
            bucket_list_db: BucketListDbConfig::default(),
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

    /// Resolve-first GC safety contract (issue #3028): when stale-bucket GC runs
    /// immediately after a ledger close that still has a live (unresolved) async
    /// merge, the merge OUTPUT file must survive `retain_buckets`. This holds only
    /// because the caller resolves pending merges BEFORE collecting the keep-set.
    ///
    /// This is the end-to-end retain assertion complementing
    /// `test_all_referenced_hashes_includes_pending_merge_output` (which covers
    /// the keep-set side only). It proves both directions:
    ///   - WITHOUT resolving first, the output hash is absent from the keep-set
    ///     and `retain_buckets` would delete the on-disk output file.
    ///   - WITH resolve-first, the output hash is in the keep-set and the file
    ///     is retained.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_resolve_first_retains_pending_merge_output() {
        use crate::bucket::Bucket;
        use crate::manager::{canonical_bucket_filename, BucketManager};
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().to_path_buf();
        let manager = BucketManager::new(dir.clone()).unwrap();

        // Two disjoint input buckets, persisted to disk and loaded disk-backed so
        // the async merge has real input files to read (and so list_buckets() sees
        // them as `.bucket.xdr` candidates for GC).
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);
        let bucket1_mem = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry1)]).unwrap();
        let bucket2_mem = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry2)]).unwrap();

        let path1 = dir.join(canonical_bucket_filename(&bucket1_mem.hash()));
        let path2 = dir.join(canonical_bucket_filename(&bucket2_mem.hash()));
        bucket1_mem.save_to_xdr_file(&path1).unwrap();
        bucket2_mem.save_to_xdr_file(&path2).unwrap();

        let bucket1 = Arc::new(Bucket::from_xdr_file_disk_backed(&path1).unwrap());
        let bucket2 = Arc::new(Bucket::from_xdr_file_disk_backed(&path2).unwrap());

        // Start a disk-backed async merge: its output is written to `dir` as
        // `<output_hash>.bucket.xdr`.
        let handle = AsyncMergeHandle::start_merge(AsyncMergeRequest {
            curr: bucket1.clone(),
            snap: bucket2.clone(),
            keep_dead_entries: DeadEntryPolicy::Remove,
            protocol_version: TEST_PROTOCOL,
            normalize_init: InitEntryPolicy::NormalizeToLive,
            shadow_buckets: vec![],
            level: 1,
            bucket_dir: Some(dir.clone()),
            counters: None,
            bucket_list_db: BucketListDbConfig::default(),
        });

        let mut bl = BucketList::new();
        bl.levels[1].next = Some(PendingMerge::Async(handle));

        // Resolve once on a throwaway clone to learn the deterministic output hash
        // and ensure the output file has been promoted to its canonical path.
        let output_hash = {
            let mut probe = BucketList::new();
            let probe_handle = AsyncMergeHandle::start_merge(AsyncMergeRequest {
                curr: bucket1.clone(),
                snap: bucket2.clone(),
                keep_dead_entries: DeadEntryPolicy::Remove,
                protocol_version: TEST_PROTOCOL,
                normalize_init: InitEntryPolicy::NormalizeToLive,
                shadow_buckets: vec![],
                level: 1,
                bucket_dir: Some(dir.clone()),
                counters: None,
                bucket_list_db: BucketListDbConfig::default(),
            });
            probe.levels[1].next = Some(PendingMerge::Async(probe_handle));
            probe.resolve_all_pending_merges().unwrap();
            probe.levels[1].next().unwrap().hash()
        };

        let output_path = dir.join(canonical_bucket_filename(&output_hash));
        assert!(
            output_path.exists(),
            "merge output file must be on disk after the merge completes"
        );

        // BEFORE resolving `bl`'s pending merge: the keep-set must NOT contain the
        // output hash (only the inputs are tracked while Pending). A retain with
        // this keep-set would delete the output file — exactly the hazard the
        // resolve-first contract prevents.
        let pre_resolve_keep = bl.all_referenced_hashes();
        assert!(
            !pre_resolve_keep.contains(&output_hash),
            "pre-resolve keep-set must omit the not-yet-resolved merge output hash"
        );

        // Now honor the resolve-first contract.
        bl.resolve_all_pending_merges().unwrap();
        let keep = bl.all_referenced_hashes();
        assert!(
            keep.contains(&output_hash),
            "post-resolve keep-set must include the merge output hash"
        );
        assert!(
            keep.contains(&bucket1.hash()) && keep.contains(&bucket2.hash()),
            "post-resolve keep-set must include the merge input hashes"
        );

        // End-to-end retain: with the resolve-first keep-set, the output file
        // survives GC.
        manager.retain_buckets(&keep).unwrap();
        assert!(
            output_path.exists(),
            "resolve-first retain_buckets must keep the merge output file"
        );

        // Counter-check: had we GC'd with the pre-resolve keep-set, the output
        // file would have been deleted (it is unreferenced from that set).
        manager.retain_buckets(&pre_resolve_keep).unwrap();
        assert!(
            !output_path.exists(),
            "GC with the pre-resolve keep-set deletes the unreferenced output — \
             confirming resolve-first is load-bearing"
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

        // Add a single archived entry at ledger 1 (must be persistent)
        let entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(Hash([1u8; 32]).into()),
                key: ScVal::Bytes(b"key1".to_vec().try_into().unwrap()),
                durability: ContractDataDurability::Persistent,
                val: ScVal::I64(100),
            }),
            ext: LedgerEntryExt::V0,
        };
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
                    keep_dead_entries: DeadEntryPolicy::Keep,
                    normalize_init: InitEntryPolicy::Preserve,
                    use_empty_curr: true, // merge(empty, snap_bucket)
                    bucket_dir: Some(&bucket_dir),
                    merge_map: None,
                    merge_counters: None,
                    bucket_list_db: BucketListDbConfig::default(),
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
                    keep_dead_entries: DeadEntryPolicy::Keep,
                    normalize_init: InitEntryPolicy::Preserve,
                    use_empty_curr: true,
                    bucket_dir: Some(&bucket_dir),
                    merge_map: None,
                    merge_counters: None,
                    bucket_list_db: BucketListDbConfig::default(),
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

        let next_states = vec![None; BUCKET_LIST_LEVELS];

        let loader = make_loader(vec![bucket0_curr, bucket1_curr]);
        let bl =
            BucketList::restore_from_has_parallel(&hashes, &next_states, loader, None).unwrap();

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

        let mut next_states = vec![None; BUCKET_LIST_LEVELS];
        next_states[0] = Some(PendingMergeState::Output(ho));

        let loader = make_loader(vec![bucket_curr, bucket_out]);
        let bl =
            BucketList::restore_from_has_parallel(&hashes, &next_states, loader, None).unwrap();

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

        let next_states = vec![None; BUCKET_LIST_LEVELS]; // all CLEAR

        let loader = make_loader(vec![bucket]);
        let bl =
            BucketList::restore_from_has_parallel(&hashes, &next_states, loader, None).unwrap();

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
        let mut next_states = vec![None; BUCKET_LIST_LEVELS];
        next_states[1] = Some(PendingMergeState::Output(hout));

        let all_buckets = vec![b0c, b0s, b2c, bout];

        let loader_seq = make_loader(all_buckets.clone());
        let bl_seq =
            BucketList::restore_from_has(&hashes, &next_states, |h| loader_seq(h)).unwrap();

        // Parametrize the parallel restore over the fan-out cap (#3245):
        // unbounded (None / Some(0)), and capped to 1, 2, 4. EVERY value must
        // produce a BYTE-IDENTICAL BucketList — same entries, same level.next
        // presence, and crucially the same bl.hash() — because capping
        // concurrency only changes HOW MANY workers materialize at once, not
        // WHAT is assembled or the assembly order (results are indexed by
        // level). This is the byte-identical-state proof.
        let seq_hash = bl_seq.hash();
        for fan_out in [None, Some(0), Some(1), Some(2), Some(4)] {
            let loader_par = make_loader(all_buckets.clone());
            let bl_par =
                BucketList::restore_from_has_parallel(&hashes, &next_states, loader_par, fan_out)
                    .unwrap();

            // Byte-identical restored state: bucketListHash must match the
            // sequential restore regardless of fan-out.
            assert_eq!(
                bl_par.hash(),
                seq_hash,
                "bl.hash() differs for fan_out={:?} — capping concurrency must \
                 not change the restored state",
                fan_out
            );

            // All entries should be reachable from both.
            for i in 1u8..=4 {
                let key = make_account_key([i; 32]);
                let seq_result = bl_seq.get(&key).unwrap();
                let par_result = bl_par.get(&key).unwrap();
                assert_eq!(
                    seq_result.is_some(),
                    par_result.is_some(),
                    "entry [{}; 32] presence mismatch (fan_out={:?})",
                    i,
                    fan_out
                );
            }

            // level.next presence must match.
            for i in 0..BUCKET_LIST_LEVELS {
                assert_eq!(
                    bl_seq.levels()[i].next.is_some(),
                    bl_par.levels()[i].next.is_some(),
                    "level {} next presence mismatch (fan_out={:?})",
                    i,
                    fan_out
                );
            }
        }
    }

    #[test]
    fn test_restore_from_has_parallel_respects_fan_out_cap() {
        // Verify the semaphore actually bounds concurrency: with fan_out =
        // Some(2) over more than 2 non-empty levels, no more than 2 loader
        // closures may execute simultaneously. We instrument the loader with an
        // atomic concurrent-counter + max-seen, and sleep inside it so that an
        // unbounded restore would reliably overlap >2 loads.
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        // Build distinct curr buckets for several levels so multiple loader
        // calls run concurrently.
        let n_levels = 6usize;
        let buckets: Vec<Bucket> = (0..n_levels)
            .map(|i| {
                let entry = make_account_entry([(i as u8) + 1; 32], (i as i64 + 1) * 100);
                Bucket::from_entries(vec![BucketListEntry::Liveentry(entry)]).unwrap()
            })
            .collect();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        for (i, b) in buckets.iter().enumerate() {
            hashes[i] = (b.hash(), Hash256::ZERO);
        }
        let next_states = vec![None; BUCKET_LIST_LEVELS];

        // Map hash -> bucket for the instrumented loader.
        let by_hash: std::collections::HashMap<Hash256, Bucket> =
            buckets.iter().map(|b| (b.hash(), b.clone())).collect();

        let current = StdArc::new(AtomicUsize::new(0));
        let max_seen = StdArc::new(AtomicUsize::new(0));
        let current_c = StdArc::clone(&current);
        let max_seen_c = StdArc::clone(&max_seen);

        let loader = move |h: &Hash256| -> Result<Bucket> {
            if h.is_zero() {
                // Sentinel path doesn't materialize a real bucket; don't count.
                return Ok(Bucket::empty());
            }
            let now = current_c.fetch_add(1, Ordering::SeqCst) + 1;
            max_seen_c.fetch_max(now, Ordering::SeqCst);
            // Hold "inside the load" briefly so concurrent loads actually overlap.
            std::thread::sleep(std::time::Duration::from_millis(20));
            let b = by_hash
                .get(h)
                .cloned()
                .expect("instrumented loader: unknown hash");
            current_c.fetch_sub(1, Ordering::SeqCst);
            Ok(b)
        };

        let cap = 2usize;
        let bl = BucketList::restore_from_has_parallel(&hashes, &next_states, loader, Some(cap))
            .unwrap();

        assert_eq!(current.load(Ordering::SeqCst), 0, "all loads completed");
        assert!(
            max_seen.load(Ordering::SeqCst) <= cap,
            "max concurrent loads {} exceeded fan_out cap {}",
            max_seen.load(Ordering::SeqCst),
            cap
        );
        // Sanity: the buckets were actually materialized.
        for (i, b) in buckets.iter().enumerate() {
            assert_eq!(bl.levels()[i].curr.hash(), b.hash());
        }
    }

    #[test]
    fn test_restore_from_has_parallel_wrong_level_count_errors() {
        // Passing wrong number of levels must return an error.
        let next_states = vec![None; BUCKET_LIST_LEVELS];
        let too_short = vec![(Hash256::ZERO, Hash256::ZERO); 5]; // < BUCKET_LIST_LEVELS

        let result = BucketList::restore_from_has_parallel(
            &too_short,
            &next_states,
            |_| unreachable!("should not call loader"),
            None,
        );
        assert!(result.is_err(), "should return error for wrong level count");
    }

    #[test]
    fn test_restore_from_has_parallel_handles_empty_hash_sentinel() {
        // A HAS that references a level via `empty_hash()` must be handled by
        // the sentinel short-circuit, not routed to the loader. We pass a
        // `make_loader(vec![])` which errors on any non-sentinel hash, so a
        // successful restore proves the short-circuit fired before the
        // loader was called.
        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[0] = (*Hash256::empty_hash(), Hash256::ZERO);

        let next_states = vec![None; BUCKET_LIST_LEVELS];

        let bl =
            BucketList::restore_from_has_parallel(&hashes, &next_states, make_loader(vec![]), None)
                .unwrap();

        assert_eq!(bl.levels()[0].curr.hash(), *Hash256::empty_hash());
        assert_eq!(bl.levels()[0].curr.len(), 0);
        assert_eq!(bl.levels()[0].snap.hash(), Hash256::ZERO);
    }

    #[test]
    fn test_restore_from_has_handles_empty_hash_sentinel() {
        // Sequential variant of the above — the same scenario via
        // `restore_from_has` instead of `restore_from_has_parallel`.
        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[0] = (*Hash256::empty_hash(), Hash256::ZERO);

        let next_states = vec![None; BUCKET_LIST_LEVELS];

        let bl = BucketList::restore_from_has(&hashes, &next_states, make_loader(vec![])).unwrap();

        assert_eq!(bl.levels()[0].curr.hash(), *Hash256::empty_hash());
        assert_eq!(bl.levels()[0].curr.len(), 0);
        assert_eq!(bl.levels()[0].snap.hash(), Hash256::ZERO);
    }

    #[test]
    fn test_restore_from_has_parallel_mixed_sentinels() {
        // Both `Hash256::ZERO` and `*Hash256::empty_hash()` must be
        // short-circuited in the same restore call. We place each sentinel
        // at distinct slots across two levels (and swap which side is which
        // between levels) to exercise both branches of `for_sentinel_hash`
        // for both curr and snap positions.
        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[0] = (*Hash256::empty_hash(), Hash256::ZERO);
        hashes[1] = (Hash256::ZERO, *Hash256::empty_hash());

        let next_states = vec![None; BUCKET_LIST_LEVELS];

        let bl =
            BucketList::restore_from_has_parallel(&hashes, &next_states, make_loader(vec![]), None)
                .unwrap();

        assert_eq!(bl.levels()[0].curr.hash(), *Hash256::empty_hash());
        assert_eq!(bl.levels()[0].curr.len(), 0);
        assert_eq!(bl.levels()[0].snap.hash(), Hash256::ZERO);

        assert_eq!(bl.levels()[1].curr.hash(), Hash256::ZERO);
        assert_eq!(bl.levels()[1].snap.hash(), *Hash256::empty_hash());
        assert_eq!(bl.levels()[1].snap.len(), 0);
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
            DeadEntryPolicy::Keep,
            TEST_PROTOCOL,
            &BucketListDbConfig::default(),
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
            DeadEntryPolicy::Keep,
            TEST_PROTOCOL,
            &BucketListDbConfig::default(),
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
        let merge_key = MergeKey::new(
            DeadEntryPolicy::Keep,
            Hash256::default(),
            Hash256::default(),
        );

        let handle = AsyncMergeHandle {
            state: MergeRecvState::Pending(receiver),
            level: 1,
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
        let merge_key = MergeKey::new(
            DeadEntryPolicy::Keep,
            Hash256::default(),
            Hash256::default(),
        );

        let handle = AsyncMergeHandle {
            state: MergeRecvState::Pending(receiver),
            level: 1,
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

    /// Regression test for AUDIT-013 + AUDIT-156: DEAD data key with alive TTL
    /// is an invariant violation. When the incremental scan encounters a stale
    /// persistent entry whose data key is shadowed by a DEAD tombstone, the
    /// lookup returns None and the scan must panic (matching stellar-core's
    /// releaseAssertOrThrow / nullptr-deref at BucketListSnapshot.cpp:756-758).
    #[test]
    #[should_panic(expected = "persistent entry not found")]
    fn test_audit_013_dead_persistent_entry_panics_during_eviction() {
        use crate::entry::get_ttl_key;

        let seed: u8 = 42;
        let contract_hash = Hash([seed; 32]);

        let data_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: contract_hash.clone(),
        });
        let stale_live_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractCode(ContractCodeEntry {
                ext: ContractCodeEntryExt::V0,
                hash: contract_hash.clone(),
                code: vec![0u8; 100].try_into().unwrap(),
            }),
            ext: LedgerEntryExt::V0,
        };

        let ttl_key = get_ttl_key(&data_key).expect("contract code should have TTL key");
        let ttl_key_hash = match &ttl_key {
            LedgerKey::Ttl(t) => t.key_hash.clone(),
            _ => panic!("expected TTL key"),
        };
        let expired_ttl_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Ttl(TtlEntry {
                key_hash: ttl_key_hash,
                live_until_ledger_seq: 50,
            }),
            ext: LedgerEntryExt::V0,
        };

        let current_ledger: u32 = 100;
        let mut bl = BucketList::new();

        // Level 0 curr: expired TTL (found by lookup during scan)
        let ttl_bucket = Bucket::from_entries(vec![
            BucketListEntry::Metaentry(BucketMetadata {
                ledger_version: TEST_PROTOCOL,
                ext: BucketMetadataExt::V0,
            }),
            BucketListEntry::Liveentry(expired_ttl_entry),
        ])
        .unwrap();
        bl.level_mut(0).unwrap().curr = Arc::new(ttl_bucket);

        // Level 1 curr: DEAD tombstone for data key (shadows the live entry)
        let dead_bucket = Bucket::from_entries(vec![
            BucketListEntry::Metaentry(BucketMetadata {
                ledger_version: TEST_PROTOCOL,
                ext: BucketMetadataExt::V0,
            }),
            BucketListEntry::Deadentry(data_key.clone()),
        ])
        .unwrap();
        bl.level_mut(1).unwrap().curr = Arc::new(dead_bucket);

        // Level 2 curr: stale LIVE entry (scan encounters this, lookup finds DEAD → None)
        let stale_bucket = Bucket::from_entries(vec![
            BucketListEntry::Metaentry(BucketMetadata {
                ledger_version: TEST_PROTOCOL,
                ext: BucketMetadataExt::V0,
            }),
            BucketListEntry::Liveentry(stale_live_entry),
        ])
        .unwrap();
        bl.level_mut(2).unwrap().curr = Arc::new(stale_bucket);

        let settings = StateArchivalSettings {
            max_entry_ttl: 1_000_000,
            min_temporary_ttl: 1,
            min_persistent_ttl: 1,
            persistent_rent_rate_denominator: 1,
            temp_rent_rate_denominator: 1,
            max_entries_to_archive: 100,
            live_soroban_state_size_window_sample_size: 0,
            live_soroban_state_size_window_sample_period: 0,
            eviction_scan_size: 1_000_000,
            starting_eviction_scan_level: 2,
        };

        let iter = crate::EvictionIterator {
            bucket_list_level: 2,
            is_curr_bucket: true,
            bucket_file_offset: 0,
        };

        // Should panic: persistent entry lookup returns None (DEAD tombstone)
        let _ = bl.scan_for_eviction_incremental(iter, current_ledger, 23, &settings);
    }

    // ============ eviction counters wiring tests (#3168) ============

    /// Build a (data entry, TTL entry) pair for a temporary contract-data key,
    /// with the TTL `live_until_ledger_seq` set to `live_until`.
    fn make_temp_contract_data_with_ttl(seed: u8, live_until: u32) -> (LedgerEntry, LedgerEntry) {
        use crate::entry::get_ttl_key;
        let data = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(Hash([seed; 32]).into()),
                key: ScVal::U32(seed as u32),
                durability: ContractDataDurability::Temporary,
                val: ScVal::Void,
            }),
            ext: LedgerEntryExt::V0,
        };
        let data_key = henyey_common::entry_to_key(&data);
        let ttl_key = get_ttl_key(&data_key).expect("contract data has TTL key");
        let key_hash = match &ttl_key {
            LedgerKey::Ttl(t) => t.key_hash.clone(),
            _ => unreachable!(),
        };
        let ttl = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Ttl(TtlEntry {
                key_hash,
                live_until_ledger_seq: live_until,
            }),
            ext: LedgerEntryExt::V0,
        };
        (data, ttl)
    }

    fn eviction_test_settings(scan_size: u32, starting_level: u32) -> StateArchivalSettings {
        StateArchivalSettings {
            max_entry_ttl: 1_000_000,
            min_temporary_ttl: 1,
            min_persistent_ttl: 1,
            persistent_rent_rate_denominator: 1,
            temp_rent_rate_denominator: 1,
            max_entries_to_archive: 100,
            live_soroban_state_size_window_sample_size: 0,
            live_soroban_state_size_window_sample_period: 0,
            eviction_scan_size: scan_size,
            starting_eviction_scan_level: starting_level,
        }
    }

    /// A scan over a non-empty bucket records bytes_scanned into the bucket
    /// list's shared eviction counters.
    #[test]
    fn test_scan_records_bytes_scanned_into_shared_counters() {
        let starting_level = 2u32;
        let current_ledger = 100u32;
        let (data, ttl) = make_temp_contract_data_with_ttl(7, 9999); // not expired

        let mut bl = BucketList::new();
        // Put the data + TTL into the starting-scan-level curr bucket so the
        // scan actually reads bytes for them.
        let bucket = Bucket::from_entries(vec![
            BucketListEntry::Metaentry(BucketMetadata {
                ledger_version: TEST_PROTOCOL,
                ext: BucketMetadataExt::V0,
            }),
            BucketListEntry::Liveentry(data),
            BucketListEntry::Liveentry(ttl),
        ])
        .unwrap();
        bl.level_mut(starting_level as usize).unwrap().curr = Arc::new(bucket);

        let settings = eviction_test_settings(1_000_000, starting_level);
        let iter = crate::EvictionIterator {
            bucket_list_level: starting_level,
            is_curr_bucket: true,
            bucket_file_offset: 0,
        };

        let result = bl
            .scan_for_eviction_incremental(iter, current_ledger, 23, &settings)
            .expect("scan should succeed");

        assert!(result.bytes_scanned > 0, "scan should read some bytes");
        assert_eq!(
            bl.eviction_counters().snapshot().bytes_scanned,
            result.bytes_scanned,
            "scan must record bytes_scanned into the shared counters"
        );
    }

    /// A scan that wraps the full bucket list publishes the cycle period into
    /// the shared counters. The first wrap is the baseline (publishes nothing),
    /// so we drive two scans across two ledgers.
    #[test]
    fn test_scan_wrap_submits_cycle_period() {
        // Empty bucket list with a tiny scan size forces a full wrap each scan
        // (every bucket finishes immediately, advancing through all levels back
        // to the starting level).
        let starting_level = 9u32; // near the top → few buckets to traverse
        let settings = eviction_test_settings(1_000_000, starting_level);
        let bl = BucketList::new();

        let iter0 = crate::EvictionIterator {
            bucket_list_level: starting_level,
            is_curr_bucket: true,
            bucket_file_offset: 0,
        };
        // First scan at ledger 100 → first wrap (baseline, publishes nothing).
        let r1 = bl
            .scan_for_eviction_incremental(iter0, 100, 23, &settings)
            .expect("scan 1 should succeed");
        assert_eq!(
            bl.eviction_counters().snapshot().eviction_cycle_period,
            0,
            "first wrap is the baseline; nothing published yet"
        );

        // Second scan at ledger 142 → second wrap → period = 142 - 100 = 42.
        let _r2 = bl
            .scan_for_eviction_incremental(r1.end_iterator, 142, 23, &settings)
            .expect("scan 2 should succeed");
        assert_eq!(
            bl.eviction_counters().snapshot().eviction_cycle_period,
            42,
            "second wrap publishes the cycle period (142 - 100)"
        );
    }

    /// A `BucketListSnapshot` shares the source list's eviction counters Arc:
    /// recording into the source is observable via the snapshot, and vice versa.
    #[test]
    fn test_snapshot_shares_eviction_counters_arc() {
        let bl = BucketList::new();
        let header = LedgerHeader {
            ledger_seq: 5,
            ..LedgerHeader::default()
        };
        let snapshot = crate::snapshot::BucketListSnapshot::new(&bl, header);

        // Record into the source list; the snapshot sees the same instance.
        bl.eviction_counters().record_incomplete_scan();
        assert_eq!(
            snapshot
                .eviction_counters()
                .snapshot()
                .incomplete_bucket_scans,
            1,
            "snapshot must share the source list's eviction counters Arc"
        );

        // And the reverse direction.
        snapshot.eviction_counters().record_bytes_scanned(512);
        assert_eq!(
            bl.eviction_counters().snapshot().bytes_scanned,
            512,
            "recording via the snapshot is visible on the source list"
        );
    }

    // ============ hash assertion tests ============

    /// Build a loader that returns a bucket with a different hash than requested,
    /// simulating a corrupted or misidentified bucket.
    fn make_wrong_hash_loader(
        correct_bucket: Bucket,
        wrong_bucket: Bucket,
    ) -> impl Fn(&Hash256) -> crate::Result<Bucket> + Send + Sync {
        let correct_hash = correct_bucket.hash();
        let map: std::collections::HashMap<Hash256, Bucket> = vec![
            (correct_bucket.hash(), correct_bucket),
            (wrong_bucket.hash(), wrong_bucket.clone()),
        ]
        .into_iter()
        .collect();
        let map = std::sync::Arc::new(map);
        // When asked for correct_hash, return wrong_bucket instead
        let wrong = std::sync::Arc::new(wrong_bucket);
        move |hash: &Hash256| {
            if *hash == correct_hash {
                Ok((*wrong).clone())
            } else {
                map.get(hash)
                    .cloned()
                    .ok_or_else(|| BucketError::Serialization("not found".to_string()))
            }
        }
    }

    #[test]
    fn test_restore_from_has_hash_mismatch_curr() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);

        let correct_bucket =
            Bucket::from_entries(vec![BucketListEntry::Liveentry(entry1)]).unwrap();
        let wrong_bucket = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry2)]).unwrap();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[0] = (correct_bucket.hash(), Hash256::ZERO);

        let next_states = vec![None; BUCKET_LIST_LEVELS];

        let loader = make_wrong_hash_loader(correct_bucket, wrong_bucket);
        let result = BucketList::restore_from_has(&hashes, &next_states, loader);
        assert!(matches!(result, Err(BucketError::HashMismatch { .. })));
    }

    #[test]
    fn test_restore_from_has_parallel_hash_mismatch_curr() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);

        let correct_bucket =
            Bucket::from_entries(vec![BucketListEntry::Liveentry(entry1)]).unwrap();
        let wrong_bucket = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry2)]).unwrap();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[0] = (correct_bucket.hash(), Hash256::ZERO);

        let next_states = vec![None; BUCKET_LIST_LEVELS];

        let loader = make_wrong_hash_loader(correct_bucket, wrong_bucket);
        let result = BucketList::restore_from_has_parallel(&hashes, &next_states, loader, None);
        assert!(matches!(result, Err(BucketError::HashMismatch { .. })));
    }

    #[test]
    fn test_restore_from_has_hash_mismatch_output() {
        let entry_curr = make_account_entry([1u8; 32], 100);
        let entry_out = make_account_entry([9u8; 32], 999);
        let entry_wrong = make_account_entry([7u8; 32], 777);

        let bucket_curr =
            Bucket::from_entries(vec![BucketListEntry::Liveentry(entry_curr)]).unwrap();
        let bucket_out = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry_out)]).unwrap();
        let bucket_wrong =
            Bucket::from_entries(vec![BucketListEntry::Liveentry(entry_wrong)]).unwrap();

        let hc = bucket_curr.hash();
        let ho = bucket_out.hash();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[0] = (hc, Hash256::ZERO);

        let mut next_states = vec![None; BUCKET_LIST_LEVELS];
        next_states[0] = Some(PendingMergeState::Output(ho));

        // Loader returns wrong bucket when output hash is requested
        let wrong_clone = bucket_wrong.clone();
        let loader = make_loader(vec![bucket_curr]);
        let output_hash = ho;
        let combined_loader = move |hash: &Hash256| {
            if *hash == output_hash {
                Ok(wrong_clone.clone())
            } else {
                loader(hash)
            }
        };

        let result = BucketList::restore_from_has(&hashes, &next_states, combined_loader);
        assert!(matches!(result, Err(BucketError::HashMismatch { .. })));
    }

    #[test]
    fn test_restore_from_has_parallel_hash_mismatch_output() {
        let entry_curr = make_account_entry([1u8; 32], 100);
        let entry_out = make_account_entry([9u8; 32], 999);
        let entry_wrong = make_account_entry([7u8; 32], 777);

        let bucket_curr =
            Bucket::from_entries(vec![BucketListEntry::Liveentry(entry_curr)]).unwrap();
        let bucket_out = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry_out)]).unwrap();
        let bucket_wrong =
            Bucket::from_entries(vec![BucketListEntry::Liveentry(entry_wrong)]).unwrap();

        let hc = bucket_curr.hash();
        let ho = bucket_out.hash();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[0] = (hc, Hash256::ZERO);

        let mut next_states = vec![None; BUCKET_LIST_LEVELS];
        next_states[0] = Some(PendingMergeState::Output(ho));

        let wrong_clone = bucket_wrong.clone();
        let loader = make_loader(vec![bucket_curr]);
        let output_hash = ho;
        let combined_loader = move |hash: &Hash256| {
            if *hash == output_hash {
                Ok(wrong_clone.clone())
            } else {
                loader(hash)
            }
        };

        let result =
            BucketList::restore_from_has_parallel(&hashes, &next_states, combined_loader, None);
        assert!(matches!(result, Err(BucketError::HashMismatch { .. })));
    }

    #[test]
    fn test_restore_from_has_next_states_under_length() {
        let hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        let next_states = vec![None; BUCKET_LIST_LEVELS - 1]; // too short

        let result = BucketList::restore_from_has(&hashes, &next_states, |_| {
            unreachable!("should not call loader")
        });
        assert!(result.is_err());

        let result = BucketList::restore_from_has_parallel(
            &hashes,
            &next_states,
            |_| unreachable!("should not call loader"),
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_restore_from_has_next_states_over_length() {
        let hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        let next_states = vec![None; BUCKET_LIST_LEVELS + 1]; // too long

        let result = BucketList::restore_from_has(&hashes, &next_states, |_| {
            unreachable!("should not call loader")
        });
        assert!(result.is_err());

        let result = BucketList::restore_from_has_parallel(
            &hashes,
            &next_states,
            |_| unreachable!("should not call loader"),
            None,
        );
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_restart_merges_from_has_next_states_under_length() {
        let mut bl = BucketList::new();
        let next_states = vec![None; BUCKET_LIST_LEVELS - 1];

        let result = bl
            .restart_merges_from_has(1, 25, &next_states, |_| unreachable!(), false)
            .await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Expected 11 next states, got 10"),
            "{err_msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_restart_merges_from_has_next_states_over_length() {
        let mut bl = BucketList::new();
        let next_states = vec![None; BUCKET_LIST_LEVELS + 1];

        let result = bl
            .restart_merges_from_has(1, 25, &next_states, |_| unreachable!(), false)
            .await;
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("Expected 11 next states, got 12"),
            "{err_msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_restart_merges_from_has_noop_with_protocol_zero() {
        // Genesis restart: protocol_version=0, all-CLEAR next states, restart_structure_based=true.
        // All prev-level snaps are empty → restart_merges breaks immediately → no-op, no panic.
        let mut bl = BucketList::new();
        let next_states = vec![None; BUCKET_LIST_LEVELS];
        bl.restart_merges_from_has(1, 0, &next_states, |_| unreachable!(), true)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_restart_merges_from_has_handles_empty_hash_sentinel() {
        // A HAS_NEXT_STATE_INPUTS entry that references inputs via the
        // empty-bucket sentinels (one ZERO, one `empty_hash()`) must be
        // handled by `load_or_sentinel` and never reach the loader. We pass
        // `make_loader(vec![])`, which errors on any non-sentinel hash, so a
        // successful restart proves both sentinel branches fired before the
        // loader was called.
        let mut bl = BucketList::new();
        let mut next_states = vec![None; BUCKET_LIST_LEVELS];
        next_states[1] = Some(PendingMergeState::Inputs {
            curr: *Hash256::empty_hash(),
            snap: Hash256::ZERO,
        });

        let result = bl
            .restart_merges_from_has(1, TEST_PROTOCOL, &next_states, make_loader(vec![]), false)
            .await;
        assert!(result.is_ok(), "restart should succeed: {:?}", result.err());
    }

    // -----------------------------------------------------------------------
    // Regression tests for #2380: missing bucket files must not be silently
    // downgraded.  Prior to the fix the restore / reconstruct paths would
    // clear the pending‐merge state when the referenced bucket was absent,
    // leading to bucket list hash divergence.  These tests verify that a
    // missing file propagates as an error.
    // -----------------------------------------------------------------------

    /// A non-zero, non-sentinel hash that no loader will recognise.
    fn phantom_hash() -> Hash256 {
        Hash256::from_bytes([0xAB; 32])
    }

    #[test]
    fn test_restore_from_has_fails_on_missing_state1_output() {
        // HAS level 1 says state=1 (output ready) with a hash that doesn't
        // exist in the loader.  restore_from_has must propagate the load error
        // rather than silently clearing the merge.
        let hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        let mut next_states = vec![None; BUCKET_LIST_LEVELS];
        next_states[1] = Some(PendingMergeState::Output(phantom_hash()));

        let result = BucketList::restore_from_has(&hashes, &next_states, |hash: &Hash256| {
            Err(BucketError::Serialization(format!(
                "bucket not found: {}",
                hash.to_hex()
            )))
        });

        assert!(
            result.is_err(),
            "restore_from_has must fail when state-1 output bucket is missing"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not found"),
            "error should mention missing bucket: {msg}"
        );
    }

    #[test]
    fn test_restore_from_has_parallel_fails_on_missing_state1_output() {
        // Same as above but via the parallel restore path.
        let hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        let mut next_states = vec![None; BUCKET_LIST_LEVELS];
        next_states[1] = Some(PendingMergeState::Output(phantom_hash()));

        let loader = |hash: &Hash256| -> crate::Result<Bucket> {
            Err(BucketError::Serialization(format!(
                "bucket not found: {}",
                hash.to_hex()
            )))
        };

        let result = BucketList::restore_from_has_parallel(&hashes, &next_states, loader, None);
        assert!(
            result.is_err(),
            "restore_from_has_parallel must fail when state-1 output bucket is missing"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_restart_merges_fails_on_missing_state2_inputs() {
        // HAS level 1 says state=2 (merge in progress) with input hashes
        // that don't exist in the loader.  restart_merges_from_has must fail.
        let mut bl = BucketList::new();
        let mut next_states = vec![None; BUCKET_LIST_LEVELS];
        next_states[1] = Some(PendingMergeState::Inputs {
            curr: phantom_hash(),
            snap: Hash256::ZERO,
        });

        let result = bl
            .restart_merges_from_has(
                1,
                TEST_PROTOCOL,
                &next_states,
                |hash: &Hash256| -> crate::Result<Bucket> {
                    Err(BucketError::Serialization(format!(
                        "bucket not found: {}",
                        hash.to_hex()
                    )))
                },
                false,
            )
            .await;

        assert!(
            result.is_err(),
            "restart_merges_from_has must fail when state-2 input bucket is missing"
        );
    }

    #[test]
    fn test_restore_from_has_succeeds_with_all_buckets_present() {
        // Positive test: state-1 output exists in the loader → restore succeeds.
        let entry = make_account_entry([42u8; 32], 500);
        let output_bucket = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry)]).unwrap();
        let output_hash = output_bucket.hash();

        let hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        let mut next_states = vec![None; BUCKET_LIST_LEVELS];
        next_states[1] = Some(PendingMergeState::Output(output_hash));

        let loader = make_loader(vec![output_bucket]);
        let result = BucketList::restore_from_has_parallel(&hashes, &next_states, loader, None);
        assert!(
            result.is_ok(),
            "restore should succeed when all buckets are present: {:?}",
            result.err()
        );

        let bl = result.unwrap();
        // Level 1 should have a pending merge (InMemory) with the output
        let merge_state = bl.levels()[1].pending_merge_state();
        assert!(
            matches!(merge_state, Some(PendingMergeState::Output(h)) if h == output_hash),
            "level 1 should have pending merge with output hash"
        );
    }

    /// Regression test for AUDIT-257 (#2300): add_batch must reject cross-category
    /// duplicate keys (same key in both init and dead entries).
    #[tokio::test(flavor = "multi_thread")]
    async fn test_add_batch_cross_category_duplicate_rejected() {
        let mut bl = BucketList::new();
        let entry = make_account_entry([5u8; 32], 100);
        let key = make_account_key([5u8; 32]);
        let result = bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![entry],
            vec![],
            vec![key],
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("duplicate or out-of-order"),
            "expected duplicate key error from cross-category init+dead"
        );
    }

    /// Regression test for AUDIT-257 (#2300): add_batch_unique must also reject
    /// cross-category duplicate keys despite skipping within-category dedup.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_add_batch_unique_cross_category_duplicate_rejected() {
        let mut bl = BucketList::new();
        let entry = make_account_entry([6u8; 32], 200);
        let key = make_account_key([6u8; 32]);
        let result = bl.add_batch_unique(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![entry],
            vec![],
            vec![key],
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("duplicate or out-of-order"),
            "expected duplicate key error from cross-category init+dead via add_batch_unique"
        );
    }

    /// Regression test for AUDIT-257 (#2300): pre-v11 protocol normalizes init
    /// entries to Liveentry, so same key in init_entries and live_entries should
    /// be caught as a duplicate.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_add_batch_pre_v11_duplicate_rejected() {
        let mut bl = BucketList::new();
        let entry = make_account_entry([7u8; 32], 100);
        let result = bl.add_batch(
            1,
            10, // pre-v11
            BucketListType::Live,
            vec![entry.clone()],
            vec![entry],
            vec![],
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("duplicate or out-of-order"),
            "expected duplicate key error for pre-v11 init+live same key"
        );
    }

    #[test]
    fn test_referenced_hashes_output() {
        let h =
            Hash256::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap();
        let state = PendingMergeState::Output(h);
        let hashes: Vec<_> = state.referenced_hashes().collect();
        assert_eq!(hashes, vec![&h]);
    }

    #[test]
    fn test_referenced_hashes_inputs() {
        let h1 =
            Hash256::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap();
        let h2 =
            Hash256::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
                .unwrap();
        let state = PendingMergeState::Inputs { curr: h1, snap: h2 };
        let hashes: Vec<_> = state.referenced_hashes().collect();
        assert_eq!(hashes, vec![&h1, &h2]);
    }

    /// Regression test for §8.2 GC safety: `all_referenced_hashes()` must include
    /// pending merge output hashes so they are not GC'd before resolution.
    #[test]
    fn test_all_referenced_hashes_includes_pending_merge_output() {
        let entry_curr = make_account_entry([1u8; 32], 100);
        let entry_out = make_account_entry([9u8; 32], 999);

        let bucket_curr =
            Bucket::from_entries(vec![BucketListEntry::Liveentry(entry_curr)]).unwrap();
        let bucket_out = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry_out)]).unwrap();

        let hc = bucket_curr.hash();
        let ho = bucket_out.hash();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[0] = (hc, Hash256::ZERO);

        let mut next_states = vec![None; BUCKET_LIST_LEVELS];
        next_states[0] = Some(PendingMergeState::Output(ho));

        let loader = make_loader(vec![bucket_curr, bucket_out]);
        let bl =
            BucketList::restore_from_has_parallel(&hashes, &next_states, loader, None).unwrap();

        let referenced = bl.all_referenced_hashes();

        // The pending merge output hash must be in the referenced set
        assert!(
            referenced.contains(&ho),
            "all_referenced_hashes() must include pending merge output hash for GC safety"
        );
        // The curr hash must also be present
        assert!(
            referenced.contains(&hc),
            "all_referenced_hashes() must include curr bucket hash"
        );
    }

    #[test]
    fn test_restore_from_has_zero_hash_output_treated_as_clear() {
        // Passing Output(Hash256::ZERO) directly to restore_from_has should
        // NOT attempt to load a bucket — it's treated as clear (no next).
        let entry = make_account_entry([1u8; 32], 100);
        let bucket = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry)]).unwrap();
        let hc = bucket.hash();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[0] = (hc, Hash256::ZERO);

        let mut next_states = vec![None; BUCKET_LIST_LEVELS];
        // Explicitly pass zero-hash output (should be treated as clear)
        next_states[0] = Some(PendingMergeState::Output(Hash256::ZERO));

        let loader = make_loader(vec![bucket]);
        let bl = BucketList::restore_from_has(&hashes, &next_states, loader).unwrap();

        // Level 0 should have NO pending merge (zero-hash output is skipped)
        assert!(
            bl.levels()[0].next.is_none(),
            "zero-hash output should be treated as clear, not loaded as pending merge"
        );
    }

    #[test]
    fn test_restore_from_has_parallel_zero_hash_output_treated_as_clear() {
        let entry = make_account_entry([1u8; 32], 100);
        let bucket = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry)]).unwrap();
        let hc = bucket.hash();

        let mut hashes = vec![(Hash256::ZERO, Hash256::ZERO); BUCKET_LIST_LEVELS];
        hashes[0] = (hc, Hash256::ZERO);

        let mut next_states = vec![None; BUCKET_LIST_LEVELS];
        next_states[0] = Some(PendingMergeState::Output(Hash256::ZERO));

        let loader = make_loader(vec![bucket]);
        let bl =
            BucketList::restore_from_has_parallel(&hashes, &next_states, loader, None).unwrap();

        assert!(
            bl.levels()[0].next.is_none(),
            "zero-hash output should be treated as clear in parallel restore"
        );
    }

    /// Regression test for issue #2498 defect A: AsyncMergeHandle::resolve() must
    /// work on a current_thread tokio runtime without panicking.
    ///
    /// Before the fix, resolve() used `block_in_place` which panics on current_thread.
    /// The fix uses a runtime-aware blocking strategy: helper thread on current_thread,
    /// block_in_place on multi_thread.
    #[tokio::test(flavor = "current_thread")]
    async fn test_async_merge_handle_resolve_on_current_thread_runtime() {
        use tokio::sync::oneshot;

        let (sender, receiver) = oneshot::channel();
        let merge_key = MergeKey::new(
            DeadEntryPolicy::Keep,
            Hash256::default(),
            Hash256::default(),
        );

        let entry = make_account_entry([42u8; 32], 500);
        let bucket = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry)]).unwrap();
        let expected_hash = bucket.hash();

        // Send the result before resolving
        sender.send(Ok(bucket)).unwrap();

        let mut handle = AsyncMergeHandle {
            state: MergeRecvState::Pending(receiver),
            level: 0,
            input_file_paths: vec![],
            input_curr_hash: Hash256::default(),
            input_snap_hash: Hash256::default(),
            merge_key,
        };

        let result = handle.resolve();
        assert!(result.is_ok(), "resolve must not panic on current_thread");
        assert_eq!(result.unwrap().hash(), expected_hash);
    }

    /// Regression test for issue #2498 defect B: resolve() must be idempotent.
    ///
    /// After the first resolve, subsequent calls must return the cached result
    /// without re-reading from the channel (which would fail since oneshot is consumed).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_async_merge_handle_resolve_idempotent() {
        use tokio::sync::oneshot;

        let (sender, receiver) = oneshot::channel();
        let merge_key = MergeKey::new(
            DeadEntryPolicy::Keep,
            Hash256::default(),
            Hash256::default(),
        );

        let entry = make_account_entry([77u8; 32], 300);
        let bucket = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry)]).unwrap();
        let expected_hash = bucket.hash();

        sender.send(Ok(bucket)).unwrap();

        let mut handle = AsyncMergeHandle {
            state: MergeRecvState::Pending(receiver),
            level: 0,
            input_file_paths: vec![],
            input_curr_hash: Hash256::default(),
            input_snap_hash: Hash256::default(),
            merge_key,
        };

        // First resolve — transitions to Ready
        let result1 = handle.resolve().unwrap();
        assert_eq!(result1.hash(), expected_hash);

        // Second resolve — returns cached result
        let result2 = handle.resolve().unwrap();
        assert_eq!(result2.hash(), expected_hash);
        assert!(Arc::ptr_eq(&result1, &result2), "must return same Arc");
    }

    /// Regression test for issue #2498: resolve() after sender is dropped (cancelled)
    /// must return an error and cache it, not panic.
    #[tokio::test(flavor = "current_thread")]
    async fn test_async_merge_handle_resolve_after_cancel() {
        use tokio::sync::oneshot;

        let (sender, receiver) = oneshot::channel();
        let merge_key = MergeKey::new(
            DeadEntryPolicy::Keep,
            Hash256::default(),
            Hash256::default(),
        );

        // Drop sender to simulate merge task cancellation
        drop(sender);

        let mut handle = AsyncMergeHandle {
            state: MergeRecvState::Pending(receiver),
            level: 0,
            input_file_paths: vec![],
            input_curr_hash: Hash256::default(),
            input_snap_hash: Hash256::default(),
            merge_key,
        };

        let result = handle.resolve();
        assert!(result.is_err(), "cancelled merge should return error");

        // Second call should return the same cached error
        let result2 = handle.resolve();
        assert!(result2.is_err(), "cached error should persist");
    }

    /// Regression test for #2499: restart-roundtrip determinism.
    ///
    /// Run A (continuous): apply N ledgers without interruption.
    /// Run B (restarted): apply M ledgers, serialize HAS, restore, restart
    /// merges, continue applying remaining N-M ledgers.
    ///
    /// At every ledger from M+1 to N, assert that Run A and Run B produce
    /// identical per-level (curr_hash, snap_hash) and overall bucket_list_hash.
    ///
    /// This catches any divergence caused by the restart_merges_from_has /
    /// restart_merges interaction with the first post-restart commit.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_restart_roundtrip_determinism() {
        // N = 256 total ledgers, M = 100 (restore point).
        // 100 ledgers is enough for:
        //   level 0: 50 spills
        //   level 1: 12 spills
        //   level 2: 3 spills
        //   level 3: 0 spills (but has pending merge from level 2 spill)
        // This ensures levels 0-2 have active curr/snap and level 3 has a pending merge.
        let total_ledgers = 256u32;
        let restore_ledger = 100u32;

        // Helper to create a unique entry for each ledger
        let make_entry = |seq: u32| -> LedgerEntry {
            let mut id = [0u8; 32];
            id[0..4].copy_from_slice(&seq.to_le_bytes());
            make_account_entry(id, seq as i64 * 10)
        };

        // ---------------------------------------------------------------
        // Run A: continuous (no restart)
        // ---------------------------------------------------------------
        let mut bl_a = BucketList::new();
        let mut hashes_a: Vec<Hash256> = Vec::with_capacity(total_ledgers as usize + 1);
        hashes_a.push(bl_a.hash()); // ledger 0

        for seq in 1..=total_ledgers {
            let entry = make_entry(seq);
            bl_a.add_batch(
                seq,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![],
                vec![entry],
                vec![],
            )
            .unwrap();
            hashes_a.push(bl_a.hash());
        }

        // ---------------------------------------------------------------
        // Run B: apply M ledgers, then serialize/restore/restart
        // ---------------------------------------------------------------
        let mut bl_b = BucketList::new();
        for seq in 1..=restore_ledger {
            let entry = make_entry(seq);
            bl_b.add_batch(
                seq,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![],
                vec![entry],
                vec![],
            )
            .unwrap();
        }

        // Sanity: hashes should match at the restore point
        assert_eq!(
            bl_b.hash(),
            hashes_a[restore_ledger as usize],
            "Hashes must match at restore point (ledger {})",
            restore_ledger
        );

        // Serialize HAS: capture per-level (curr_hash, snap_hash) and next state.
        // Importantly, we do NOT call resolve_all_pending_merges() here: this
        // mirrors production where the HAS is built while merges may still be
        // in-progress (state=2 with input hashes). The restore path must
        // re-merge from those inputs and produce the same result.
        let mut has_hashes: Vec<(Hash256, Hash256)> = Vec::new();
        let mut has_next_states: Vec<Option<PendingMergeState>> = Vec::new();
        let mut all_buckets: Vec<Bucket> = Vec::new();

        for level in bl_b.levels() {
            let curr_hash = level.curr.hash();
            let snap_hash = level.snap.hash();
            has_hashes.push((curr_hash, snap_hash));

            let merge_state = level.pending_merge_state();
            has_next_states.push(merge_state.clone());

            // Collect all non-empty buckets for the loader
            if !level.curr.is_empty() {
                all_buckets.push((*level.curr).clone());
            }
            if !level.snap.is_empty() {
                all_buckets.push((*level.snap).clone());
            }

            // Collect merge output bucket if state=1
            if let Some(PendingMergeState::Output(ref h)) = merge_state {
                if let Some(PendingMerge::InMemory(ref bucket)) = level.next {
                    if bucket.hash() == *h {
                        all_buckets.push(bucket.clone());
                    }
                }
                if let Some(PendingMerge::Async(ref handle)) = level.next {
                    if let MergeRecvState::Ready(Ok(ref bucket)) = handle.state {
                        if bucket.hash() == *h {
                            all_buckets.push((**bucket).clone());
                        }
                    }
                }
            }

            // For state=2 (inputs), the input buckets are curr/snap from a
            // lower level or from this level's own curr/snap. They should
            // already be collected above or loadable by hash from the map.
            // Also explicitly collect the input hashes from the async handle.
            if let Some(PendingMerge::Async(ref handle)) = level.next {
                // The input buckets might be different from this level's curr/snap
                // (they are the previous snap from the level below). Collect them
                // by following the handle's stored input hashes.
                for input_hash in [&handle.input_curr_hash, &handle.input_snap_hash] {
                    if !input_hash.is_zero() {
                        // Try to find this bucket in levels below
                        for other_level in bl_b.levels() {
                            if other_level.curr.hash() == *input_hash {
                                all_buckets.push((*other_level.curr).clone());
                            }
                            if other_level.snap.hash() == *input_hash {
                                all_buckets.push((*other_level.snap).clone());
                            }
                        }
                    }
                }
            }
        }

        // Verify we have at least one non-clear next state (state=1 or state=2)
        let has_active_next = has_next_states.iter().any(|s| s.is_some());
        assert!(
            has_active_next,
            "HAS must contain at least one active next state for restart test to be meaningful"
        );

        // Verify the test exercises the state=2 (re-merge from inputs) path.
        // Without resolve_all_pending_merges() before capture, async merges
        // that haven't completed yet produce state=2 in the HAS.
        let state2_count = has_next_states
            .iter()
            .filter(|s| matches!(s, Some(PendingMergeState::Inputs { .. })))
            .count();
        assert!(
            state2_count > 0,
            "HAS must contain at least one state=2 level to exercise the re-merge path"
        );

        // Restore from HAS
        let loader = std::sync::Arc::new(make_loader(all_buckets));
        let loader_ref = loader.clone();
        let mut bl_restored = BucketList::restore_from_has_parallel(
            &has_hashes,
            &has_next_states,
            move |h: &Hash256| loader_ref(h),
            None,
        )
        .unwrap();
        bl_restored.set_ledger_seq(restore_ledger);

        // Restart merges (both HAS-based and structure-based)
        let loader_ref2 = loader.clone();
        bl_restored
            .restart_merges_from_has(
                restore_ledger,
                TEST_PROTOCOL,
                &has_next_states,
                |hash: &Hash256| loader_ref2(hash),
                true, // restart_structure_based
            )
            .await
            .unwrap();

        // Resolve all pending merges in the restored bucket list
        bl_restored.resolve_all_pending_merges().unwrap();

        // Verify hash matches at restore point
        // Note: the hash only depends on curr/snap, not next, so it should match.
        assert_eq!(
            bl_restored.hash(),
            hashes_a[restore_ledger as usize],
            "Restored bucket list hash must match at restore point"
        );

        // ---------------------------------------------------------------
        // Continue Run B from the restore point
        // ---------------------------------------------------------------
        for seq in (restore_ledger + 1)..=total_ledgers {
            let entry = make_entry(seq);
            bl_restored
                .add_batch(
                    seq,
                    TEST_PROTOCOL,
                    BucketListType::Live,
                    vec![],
                    vec![entry],
                    vec![],
                )
                .unwrap();

            let hash_a = &hashes_a[seq as usize];
            let hash_b = bl_restored.hash();
            assert_eq!(
                *hash_a,
                hash_b,
                "RESTART DIVERGENCE at ledger {}: continuous={} restarted={}",
                seq,
                hash_a.to_hex(),
                hash_b.to_hex()
            );
        }
    }

    /// Regression test for #2503: restart-roundtrip with disk-backed merges
    /// and overlapping keys.
    ///
    /// The production bug manifests only after fresh catchup from archive, where
    /// the restored bucket list uses disk-backed merges (bucket_dir is set).
    /// The existing test_restart_roundtrip_determinism uses unique keys and
    /// in-memory merges, masking potential divergence in the disk merge path.
    ///
    /// This test:
    /// 1. Uses overlapping keys (same entries updated across many ledgers)
    ///    so merges must resolve key conflicts.
    /// 2. Sets bucket_dir on the restored bucket list to exercise disk-backed
    ///    merging in both restart_merges and subsequent add_batch_internal.
    /// 3. Compares per-level hashes at every ledger after restore to catch
    ///    the first point of divergence.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_restart_roundtrip_disk_backed_overlapping_keys() {
        let total_ledgers = 512u32;
        let restore_ledger = 128u32; // checkpoint boundary (multiple of 64)

        // Use a SMALL pool of 8 account IDs, updated round-robin.
        // This guarantees merges encounter duplicate keys.
        let account_ids: Vec<[u8; 32]> = (0..8u8)
            .map(|i| {
                let mut id = [0u8; 32];
                id[0] = i;
                id
            })
            .collect();

        let make_entry = |seq: u32| -> LedgerEntry {
            let idx = (seq as usize) % account_ids.len();
            make_account_entry(account_ids[idx], seq as i64 * 10)
        };

        // ---------------------------------------------------------------
        // Run A: continuous (no restart), in-memory (no bucket_dir)
        // ---------------------------------------------------------------
        let mut bl_a = BucketList::new();
        let mut hashes_a: Vec<Hash256> = Vec::with_capacity(total_ledgers as usize + 1);
        hashes_a.push(bl_a.hash());

        for seq in 1..=total_ledgers {
            let entry = make_entry(seq);
            bl_a.add_batch(
                seq,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![],
                vec![entry],
                vec![],
            )
            .unwrap();
            hashes_a.push(bl_a.hash());
        }

        // ---------------------------------------------------------------
        // Run B: restore + restart with disk-backed merges
        // ---------------------------------------------------------------
        let mut bl_b = BucketList::new();
        for seq in 1..=restore_ledger {
            let entry = make_entry(seq);
            bl_b.add_batch(
                seq,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![],
                vec![entry],
                vec![],
            )
            .unwrap();
        }

        assert_eq!(
            bl_b.hash(),
            hashes_a[restore_ledger as usize],
            "Hashes must match at restore point"
        );

        // Capture HAS
        let mut has_hashes: Vec<(Hash256, Hash256)> = Vec::new();
        let mut has_next_states: Vec<Option<PendingMergeState>> = Vec::new();
        let mut all_buckets: Vec<Bucket> = Vec::new();

        for level in bl_b.levels() {
            has_hashes.push((level.curr.hash(), level.snap.hash()));
            let merge_state = level.pending_merge_state();
            has_next_states.push(merge_state.clone());

            if !level.curr.is_empty() {
                all_buckets.push((*level.curr).clone());
            }
            if !level.snap.is_empty() {
                all_buckets.push((*level.snap).clone());
            }
            if let Some(PendingMergeState::Output(ref h)) = merge_state {
                if let Some(PendingMerge::InMemory(ref bucket)) = level.next {
                    if bucket.hash() == *h {
                        all_buckets.push(bucket.clone());
                    }
                }
                if let Some(PendingMerge::Async(ref handle)) = level.next {
                    if let MergeRecvState::Ready(Ok(ref bucket)) = handle.state {
                        if bucket.hash() == *h {
                            all_buckets.push((**bucket).clone());
                        }
                    }
                }
            }
            if let Some(PendingMerge::Async(ref handle)) = level.next {
                for input_hash in [&handle.input_curr_hash, &handle.input_snap_hash] {
                    if !input_hash.is_zero() {
                        for other_level in bl_b.levels() {
                            if other_level.curr.hash() == *input_hash {
                                all_buckets.push((*other_level.curr).clone());
                            }
                            if other_level.snap.hash() == *input_hash {
                                all_buckets.push((*other_level.snap).clone());
                            }
                        }
                    }
                }
            }
        }

        // Set up bucket_dir FIRST (before restoring), matching production
        let bucket_dir = std::env::current_dir()
            .unwrap()
            .join("target")
            .join("test-buckets-2503");
        let _ = std::fs::remove_dir_all(&bucket_dir);
        std::fs::create_dir_all(&bucket_dir).unwrap();

        // Save all buckets to disk as uncompressed XDR files (matching production
        // where buckets are downloaded from the archive and stored on disk).
        for bucket in &all_buckets {
            if !bucket.is_empty() {
                let path = bucket_dir.join(super::canonical_bucket_filename(&bucket.hash()));
                if !path.exists() {
                    bucket.save_to_xdr_file(&path).unwrap();
                }
            }
        }

        // Create a loader that loads DiskBacked buckets from the bucket_dir,
        // matching the production catchup path (Bucket::from_xdr_file_disk_backed).
        let disk_loader_dir = bucket_dir.clone();
        let disk_loader = move |hash: &Hash256| -> crate::Result<Bucket> {
            if hash.is_zero() {
                return Ok(Bucket::empty());
            }
            let path = disk_loader_dir.join(super::canonical_bucket_filename(hash));
            Bucket::from_xdr_file_disk_backed(&path)
        };

        // Restore from HAS using DiskBacked buckets (matching production)
        let disk_loader_clone = {
            let dir = bucket_dir.clone();
            move |hash: &Hash256| -> crate::Result<Bucket> {
                if hash.is_zero() {
                    return Ok(Bucket::empty());
                }
                let path = dir.join(super::canonical_bucket_filename(hash));
                Bucket::from_xdr_file_disk_backed(&path)
            }
        };
        let mut bl_restored = BucketList::restore_from_has_parallel(
            &has_hashes,
            &has_next_states,
            disk_loader_clone,
            None,
        )
        .unwrap();
        bl_restored.set_ledger_seq(restore_ledger);

        // Set bucket_dir to enable disk-backed merges (the production condition)
        bl_restored.set_bucket_dir(bucket_dir.clone());

        // Restart merges (structure-based, matching production)
        bl_restored
            .restart_merges_from_has(
                restore_ledger,
                TEST_PROTOCOL,
                &has_next_states,
                disk_loader,
                true,
            )
            .await
            .unwrap();

        // Do NOT call resolve_all_pending_merges() here — in production,
        // pending merges stay pending until commit() is called during the next
        // spill. This matches the production flow more closely.

        // Verify hash at restore point
        assert_eq!(
            bl_restored.hash(),
            hashes_a[restore_ledger as usize],
            "Restored bucket list hash must match at restore point (disk-backed)"
        );

        // Continue from restore point with disk-backed merges
        for seq in (restore_ledger + 1)..=total_ledgers {
            let entry = make_entry(seq);
            bl_restored
                .add_batch(
                    seq,
                    TEST_PROTOCOL,
                    BucketListType::Live,
                    vec![],
                    vec![entry],
                    vec![],
                )
                .unwrap();

            let hash_a = &hashes_a[seq as usize];
            let hash_b = bl_restored.hash();
            if *hash_a != hash_b {
                // Detailed per-level comparison for diagnostics
                for (i, (level_a, level_b)) in bl_a
                    .levels()
                    .iter()
                    .zip(bl_restored.levels().iter())
                    .enumerate()
                {
                    if level_a.curr.hash() != level_b.curr.hash() {
                        eprintln!(
                            "  Level {} CURR diverges: continuous={} restarted={}",
                            i,
                            level_a.curr.hash().to_hex(),
                            level_b.curr.hash().to_hex()
                        );
                    }
                    if level_a.snap.hash() != level_b.snap.hash() {
                        eprintln!(
                            "  Level {} SNAP diverges: continuous={} restarted={}",
                            i,
                            level_a.snap.hash().to_hex(),
                            level_b.snap.hash().to_hex()
                        );
                    }
                }
                panic!(
                    "DISK-BACKED RESTART DIVERGENCE at ledger {}: continuous={} restarted={}",
                    seq,
                    hash_a.to_hex(),
                    hash_b.to_hex()
                );
            }
        }

        // Cleanup test bucket directory
        let _ = std::fs::remove_dir_all(&bucket_dir);
    }

    // ========================================================================
    // Regression tests for #3478 — ENOSPC during bucket merge is recoverable,
    // not corruption. The validator must NOT panic/abort on a transient disk-
    // full (no partial state committed), but MUST stay fatal on genuine
    // corruption (HashMismatch / Corruption). Classification keys on the raw
    // errno (ENOSPC=28, EDQUOT=122), NOT on string-matching.
    // ========================================================================

    /// The exact production ENOSPC error observed on mainnet (session
    /// 74535976): `IO error: No space left on device (os error 28)`.
    /// ENOSPC is errno 28 on Linux.
    fn make_enospc_bucket_error() -> BucketError {
        BucketError::Io(std::io::Error::from_raw_os_error(28))
    }

    /// A transient-IO merge whose result is the production ENOSPC error must
    /// surface `Err` AND classify as `is_transient_io() == true` — proving the
    /// errno survives the merge error chain (it was stringified away on main).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_enospc_merge_failure_is_recoverable_not_corruption_3478() {
        let mut level = BucketLevel::new(1);

        // Inject a level-1 async merge whose result is the exact production
        // ENOSPC error, via the test-only structured-errno ctor.
        let merge_err =
            crate::merge_map::MergeError::from_bucket_error(&make_enospc_bucket_error());
        let handle = AsyncMergeHandle::new_failed_for_test(1, merge_err);
        level.next = Some(PendingMerge::Async(handle));

        let mut bl = BucketList::new();
        bl.levels[1] = level;

        let err = bl
            .resolve_all_pending_merges()
            .expect_err("ENOSPC merge failure must surface an error");

        assert!(
            err.is_transient_io(),
            "ENOSPC (errno 28) must classify as transient IO (recoverable), got: {err:?}"
        );
    }

    /// Genuine corruption (HashMismatch / Corruption) must STAY fatal — it must
    /// NOT be classified as transient IO, so it is never masked/recovered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_corruption_merge_failure_stays_fatal_3478() {
        // HashMismatch
        let hash_mismatch = BucketError::HashMismatch {
            expected: "aa".to_string(),
            actual: "bb".to_string(),
        };
        assert!(
            !hash_mismatch.is_transient_io(),
            "HashMismatch must NOT be transient IO (stays fatal)"
        );

        // Corruption
        let corruption = BucketError::Corruption("entries not sorted".to_string());
        assert!(
            !corruption.is_transient_io(),
            "Corruption must NOT be transient IO (stays fatal)"
        );

        // EIO (errno 5) is conservatively NOT transient (could be hardware
        // corruption).
        let eio = BucketError::Io(std::io::Error::from_raw_os_error(5));
        assert!(
            !eio.is_transient_io(),
            "EIO must NOT be transient IO (conservative — could be corruption)"
        );

        // And the round-trip through MergeError preserves corruption class.
        let merge_err = crate::merge_map::MergeError::from_bucket_error(&corruption);
        let mut level = BucketLevel::new(1);
        let handle = AsyncMergeHandle::new_failed_for_test(1, merge_err);
        level.next = Some(PendingMerge::Async(handle));
        let mut bl = BucketList::new();
        bl.levels[1] = level;
        let err = bl
            .resolve_all_pending_merges()
            .expect_err("corruption merge failure must surface an error");
        assert!(
            !err.is_transient_io(),
            "corruption round-tripped through MergeError must stay fatal, got: {err:?}"
        );
    }

    /// A transient-IO-failed level must NOT poison its cache: once "disk
    /// recovers", a subsequent real re-prepare at a later ledger must re-issue
    /// the merge and complete cleanly. On main the failed merge caches
    /// `Ready(Err)` permanently and the level would wedge forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_failed_level_remerges_after_recovery_3478() {
        let mut bl = BucketList::new();

        // Inject a transient-IO (ENOSPC) failed async merge into level 1.
        let merge_err =
            crate::merge_map::MergeError::from_bucket_error(&make_enospc_bucket_error());
        let handle = AsyncMergeHandle::new_failed_for_test(1, merge_err);
        bl.levels[1].next = Some(PendingMerge::Async(handle));

        // Resolving surfaces the transient-IO error.
        let err = bl
            .resolve_all_pending_merges()
            .expect_err("ENOSPC merge failure must surface an error");
        assert!(err.is_transient_io(), "expected transient IO, got {err:?}");

        // After a transient-IO failure the level must be reset so the NEXT
        // close can re-issue a fresh merge — NOT left holding a poisoned
        // `Ready(Err)`. Drive a REAL re-prepare and assert it succeeds.
        assert!(
            bl.levels[1].next.is_none(),
            "transient-IO-failed level must reset its pending merge so the next close re-issues it"
        );

        // "Disk recovers": drive a real re-prepare on level 1 (in-memory merge,
        // no bucket_dir) and assert it completes cleanly.
        let incoming = Arc::new(Bucket::empty());
        bl.levels[1]
            .prepare_with_normalization(
                TEST_PROTOCOL,
                incoming,
                &[],
                MergeContext {
                    keep_dead_entries: DeadEntryPolicy::Keep,
                    normalize_init: InitEntryPolicy::Preserve,
                    use_empty_curr: false,
                    bucket_dir: None,
                    merge_map: None,
                    merge_counters: None,
                    bucket_list_db: BucketListDbConfig::default(),
                },
            )
            .expect("re-prepare after recovery must start a fresh merge");

        bl.resolve_all_pending_merges()
            .expect("re-issued merge must resolve cleanly after disk recovery");
    }

    // ============ #3867: non-blocking pre-resolution of background merges ============
    //
    // The event loop parked 15-100s in the `broadcast` phase because a deep-level
    // (>=6) background merge that had already finished was only *observed* by the
    // blocking `resolve()` at its commit boundary, holding the BucketList write
    // lock for the whole remaining merge. `poll_pending_merges()` observes such a
    // completed merge eagerly and non-blockingly so the boundary `commit()` takes
    // the instant `Ready` fast path. These tests do not exist on `origin/main`
    // (the `try_resolve`/`poll_pending_merges` API is added by this fix), so the
    // module fails to compile there — the failing-first signal for the regression.

    /// A completed, real disk-backed async merge is cached `Ready` (HAS state 1
    /// `Output`) by a non-blocking `poll_pending_merges()`, with no blocking
    /// `resolve()`. On main a finished-but-unobserved merge stays state 2
    /// (`Inputs`) until a blocking `resolve()`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_poll_pending_merges_caches_completed_async_merge() {
        use crate::bucket::Bucket;
        use crate::manager::canonical_bucket_filename;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path().to_path_buf();

        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);
        let bucket1_mem = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry1)]).unwrap();
        let bucket2_mem = Bucket::from_entries(vec![BucketListEntry::Liveentry(entry2)]).unwrap();

        let path1 = dir.join(canonical_bucket_filename(&bucket1_mem.hash()));
        let path2 = dir.join(canonical_bucket_filename(&bucket2_mem.hash()));
        bucket1_mem.save_to_xdr_file(&path1).unwrap();
        bucket2_mem.save_to_xdr_file(&path2).unwrap();

        let bucket1 = Arc::new(Bucket::from_xdr_file_disk_backed(&path1).unwrap());
        let bucket2 = Arc::new(Bucket::from_xdr_file_disk_backed(&path2).unwrap());

        // Learn the deterministic merge output hash on a throwaway probe (via a
        // blocking resolve) so we can assert the polled result matches it.
        let expected_output_hash = {
            let mut probe = BucketList::new();
            let probe_handle = AsyncMergeHandle::start_merge(AsyncMergeRequest {
                curr: bucket1.clone(),
                snap: bucket2.clone(),
                keep_dead_entries: DeadEntryPolicy::Remove,
                protocol_version: TEST_PROTOCOL,
                normalize_init: InitEntryPolicy::NormalizeToLive,
                shadow_buckets: vec![],
                level: 1,
                bucket_dir: Some(dir.clone()),
                counters: None,
                bucket_list_db: BucketListDbConfig::default(),
            });
            probe.levels[1].next = Some(PendingMerge::Async(probe_handle));
            probe.resolve_all_pending_merges().unwrap();
            probe.levels[1].next().unwrap().hash()
        };

        // The system under test: a level whose async merge runs in the background.
        let handle = AsyncMergeHandle::start_merge(AsyncMergeRequest {
            curr: bucket1.clone(),
            snap: bucket2.clone(),
            keep_dead_entries: DeadEntryPolicy::Remove,
            protocol_version: TEST_PROTOCOL,
            normalize_init: InitEntryPolicy::NormalizeToLive,
            shadow_buckets: vec![],
            level: 1,
            bucket_dir: Some(dir.clone()),
            counters: None,
            bucket_list_db: BucketListDbConfig::default(),
        });
        let mut bl = BucketList::new();
        bl.levels[1].next = Some(PendingMerge::Async(handle));

        // Drive completion with a bounded NON-BLOCKING poll loop (never resolve()).
        // The merge is tiny and finishes quickly; poll caches it as state 1.
        let mut polled_state = None;
        for _ in 0..500 {
            bl.poll_pending_merges();
            polled_state = bl.levels[1].pending_merge_state();
            if matches!(polled_state, Some(PendingMergeState::Output(_))) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(
            polled_state,
            Some(PendingMergeState::Output(expected_output_hash)),
            "poll_pending_merges() must cache a completed async merge as HAS state 1 (Output) \
             non-blockingly"
        );

        // The subsequent commit must take the already-Ready fast path and promote
        // the pre-cached output into curr.
        let recorded = bl.levels[1].commit().unwrap();
        assert!(
            recorded.is_some(),
            "commit() of a pre-resolved async merge must still record the completed merge"
        );
        assert_eq!(
            bl.levels[1].curr.hash(),
            expected_output_hash,
            "commit() must promote the pre-cached merge output into curr"
        );
    }

    /// `try_resolve()` on a still-running async merge leaves it `Pending`
    /// (HAS state 2) and never blocks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_try_resolve_async_empty_leaves_pending() {
        let (sender, receiver) = oneshot::channel::<Result<Bucket>>();
        let merge_key = MergeKey::new(
            DeadEntryPolicy::Keep,
            Hash256::default(),
            Hash256::default(),
        );
        let mut handle = AsyncMergeHandle {
            state: MergeRecvState::Pending(receiver),
            level: 6,
            input_file_paths: vec![],
            input_curr_hash: Hash256::from_bytes([7u8; 32]),
            input_snap_hash: Hash256::from_bytes([8u8; 32]),
            merge_key,
        };

        handle.try_resolve();
        assert!(
            matches!(handle.state, MergeRecvState::Pending(_)),
            "try_resolve() must leave an unfinished merge Pending (never block)"
        );

        let mut level = BucketLevel::new(6);
        level.next = Some(PendingMerge::Async(handle));
        level.poll_pending_merge();
        assert_eq!(
            level.pending_merge_state(),
            Some(PendingMergeState::Inputs {
                curr: Hash256::from_bytes([7u8; 32]),
                snap: Hash256::from_bytes([8u8; 32]),
            }),
            "an unfinished merge must still serialize as HAS state 2 (Inputs) after a poll"
        );

        // keep the sender alive until here so the channel isn't Closed
        drop(sender);
    }

    /// A dropped sender (cancelled merge) makes `try_resolve()` cache the EXACT
    /// same classified `MergeError` that a blocking `resolve()` produces for the
    /// same closed channel — the two observation paths must not diverge (#3478).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_try_resolve_async_closed_caches_classified_error() {
        // Reference: what does a blocking resolve() yield on a closed channel?
        let (ref_sender, ref_receiver) = oneshot::channel::<Result<Bucket>>();
        let mut ref_handle = AsyncMergeHandle {
            state: MergeRecvState::Pending(ref_receiver),
            level: 6,
            input_file_paths: vec![],
            input_curr_hash: Hash256::default(),
            input_snap_hash: Hash256::default(),
            merge_key: MergeKey::new(
                DeadEntryPolicy::Keep,
                Hash256::default(),
                Hash256::default(),
            ),
        };
        drop(ref_sender);
        let ref_err = ref_handle
            .resolve()
            .expect_err("resolve() on a closed channel must error");

        // Under test: try_resolve() on an equivalently-closed channel.
        let (sender, receiver) = oneshot::channel::<Result<Bucket>>();
        let mut handle = AsyncMergeHandle {
            state: MergeRecvState::Pending(receiver),
            level: 6,
            input_file_paths: vec![],
            input_curr_hash: Hash256::default(),
            input_snap_hash: Hash256::default(),
            merge_key: MergeKey::new(
                DeadEntryPolicy::Keep,
                Hash256::default(),
                Hash256::default(),
            ),
        };
        drop(sender);
        handle.try_resolve();

        // The cached error must be terminal and re-surface identically to resolve().
        let try_err = handle
            .resolve()
            .expect_err("a Closed channel cached by try_resolve() must stay an error");
        assert_eq!(
            try_err.to_string(),
            ref_err.to_string(),
            "try_resolve()'s Closed classification must match resolve()'s exactly"
        );
        assert_eq!(
            std::mem::discriminant(&try_err),
            std::mem::discriminant(&ref_err),
            "try_resolve()'s Closed BucketError variant must match resolve()'s"
        );
    }

    /// `SharedMergeHandle::try_resolve()` leaves the handle unresolved while the
    /// producer is silent, then caches once the producer publishes a result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_try_resolve_shared_pending_then_ready() {
        use crate::merge_map::SharedMergeMetadata;
        use tokio::sync::watch;

        let (sender, receiver) = watch::channel::<Option<MergeResult>>(None);
        let metadata = SharedMergeMetadata {
            merge_key: MergeKey::new(
                DeadEntryPolicy::Keep,
                Hash256::default(),
                Hash256::default(),
            ),
            input_curr_hash: Hash256::from_bytes([3u8; 32]),
            input_snap_hash: Hash256::from_bytes([4u8; 32]),
            input_file_paths: vec![],
            level: 6,
        };
        let mut handle = SharedMergeHandle::new(receiver, metadata);

        handle.try_resolve();
        assert!(
            handle.cached_result.is_none(),
            "try_resolve() must leave a silent shared merge unresolved (never block)"
        );

        let bucket = Arc::new(
            Bucket::from_entries(vec![BucketListEntry::Liveentry(make_account_entry(
                [5u8; 32], 500,
            ))])
            .unwrap(),
        );
        sender.send(Some(Ok(bucket.clone()))).unwrap();

        handle.try_resolve();
        assert!(
            handle.cached_result.is_some(),
            "try_resolve() must cache the shared merge result once published"
        );
        assert_eq!(
            handle.resolve().unwrap().hash(),
            bucket.hash(),
            "the cached shared result must be returned by a subsequent resolve()"
        );
    }

    /// `poll_pending_merges()` is idempotent and a no-op on levels with no
    /// async/shared merge (absent or InMemory).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_poll_pending_merges_idempotent_and_noop_on_empty_level() {
        let mut bl = BucketList::new();

        // No pending merge anywhere: poll is a harmless no-op, repeatable.
        let before = bl.hash();
        bl.poll_pending_merges();
        bl.poll_pending_merges();
        assert_eq!(
            bl.hash(),
            before,
            "poll on empty levels must not change state"
        );

        // InMemory merge (level 0 style): poll must not touch it.
        let inmem = Bucket::from_entries(vec![BucketListEntry::Liveentry(make_account_entry(
            [9u8; 32], 42,
        ))])
        .unwrap();
        let inmem_hash = inmem.hash();
        bl.levels[0].next = Some(PendingMerge::InMemory(inmem));
        bl.poll_pending_merges();
        bl.poll_pending_merges();
        assert_eq!(
            bl.levels[0].pending_merge_state(),
            Some(PendingMergeState::Output(inmem_hash)),
            "poll must leave an InMemory merge untouched (already state 1)"
        );
    }

    /// Parity guard (Critic B): a non-blocking poll interleaved with `add_batch`
    /// across a spill boundary yields a BYTE-IDENTICAL bucket-list hash and
    /// per-level curr/snap hashes vs the same add sequence WITHOUT the poll. The
    /// poll only changes *when* a finished merge is observed, never the committed
    /// bytes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_add_batch_commit_nonblocking_after_poll() {
        // Reference run: no interleaved poll.
        let mut reference = BucketList::new();
        for seq in 1..=8u32 {
            let entry = make_account_entry([seq as u8; 32], seq as i64 * 100);
            reference
                .add_batch(
                    seq,
                    TEST_PROTOCOL,
                    BucketListType::Live,
                    vec![entry],
                    vec![],
                    vec![],
                )
                .unwrap();
        }
        reference.resolve_all_pending_merges().unwrap();
        let ref_hash = reference.hash();
        let ref_levels: Vec<(Hash256, Hash256)> = reference
            .levels
            .iter()
            .map(|l| (l.curr.hash(), l.snap.hash()))
            .collect();

        // Polled run: the same adds, but poll (non-blockingly) between each,
        // giving any finished background merge a chance to be cached as state 1
        // before its boundary commit.
        let mut polled = BucketList::new();
        let mut saw_precached_output = false;
        for seq in 1..=8u32 {
            let entry = make_account_entry([seq as u8; 32], seq as i64 * 100);
            polled
                .add_batch(
                    seq,
                    TEST_PROTOCOL,
                    BucketListType::Live,
                    vec![entry],
                    vec![],
                    vec![],
                )
                .unwrap();
            // Let any in-flight background merge settle, then poll.
            for _ in 0..50 {
                polled.poll_pending_merges();
                if polled
                    .levels
                    .iter()
                    .any(|l| matches!(l.pending_merge_state(), Some(PendingMergeState::Output(_))))
                {
                    saw_precached_output = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }
        polled.resolve_all_pending_merges().unwrap();

        assert_eq!(
            polled.hash(),
            ref_hash,
            "poll interleaving must not change the bucket-list hash (parity guard)"
        );
        let polled_levels: Vec<(Hash256, Hash256)> = polled
            .levels
            .iter()
            .map(|l| (l.curr.hash(), l.snap.hash()))
            .collect();
        assert_eq!(
            polled_levels, ref_levels,
            "poll interleaving must not change any per-level curr/snap hash (parity guard)"
        );
        assert!(
            saw_precached_output,
            "at least one background merge must have been observed as pre-cached state 1 by \
             poll before its boundary commit"
        );
    }
}
