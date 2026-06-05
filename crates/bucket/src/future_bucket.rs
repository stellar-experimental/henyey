//! FutureBucket - Async bucket merging support.
//!
//! FutureBucket is a wrapper around async bucket merge operations, supporting
//! both in-progress and completed merges. It enables:
//!
//! - Background merge execution via async tasks
//! - Serialization/deserialization of merge state to HistoryArchiveState
//! - Restarting merges after deserialization
//!
//! # State Machine
//!
//! FutureBucket has five states:
//!
//! - `Clear`: No inputs or outputs
//! - `HashOutput`: Has output hash, but no live bucket reference
//! - `HashInputs`: Has input hashes, but no live bucket references
//! - `LiveOutput`: Has resolved output bucket
//! - `LiveMerging`: Merge in progress with live input references
//!
//! # Lifecycle
//!
//! 1. Created with inputs → starts merge → `LiveMerging`
//! 2. Merge completes → `resolve()` → `LiveOutput`
//! 3. Can be serialized (captures hashes) → `HashInputs` or `HashOutput`
//! 4. Can be deserialized and made live again → `makeLive()`

use std::sync::Arc;
use tokio::sync::oneshot;

use henyey_common::Hash256;
use serde::{Deserialize, Serialize};

use crate::bucket::Bucket;
use crate::merge::{merge_buckets, DeadEntryPolicy, InitEntryPolicy, MergeOptions};
use crate::{BucketError, Result};

/// Load a bucket via the provided closure and verify the hash matches.
fn load_and_verify<F>(hash: &Hash256, load_bucket: &F) -> Result<Bucket>
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

/// State of a FutureBucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FutureBucketState {
    /// No inputs; no outputs; no hashes.
    Clear,
    /// Output hash present; no live output bucket.
    HashOutput,
    /// Input hashes present; no live input buckets.
    HashInputs,
    /// Live output bucket available.
    LiveOutput,
    /// Live input buckets; merge in progress.
    LiveInputs,
}

/// Key identifying a unique merge operation.
///
/// Used to deduplicate concurrent merge requests for the same inputs.
///
/// Spec: BUCKETLISTDB_SPEC §7.3 — MergeKey intentionally omits shadowHashes,
/// normalize_init, and protocol_version fields present in stellar-core's MergeKey.
/// Shadow buckets were removed in protocol 12; henyey supports P24+ only, so shadow
/// vectors are always empty and excluded from merge identity. The normalize_init and
/// protocol_version fields are predictable under P24+ scope. This P24+ scope waiver
/// applies to all merge-key consumers: FutureBucket::merge_key(),
/// prepare_with_normalization, and HAS restart paths.
/// See also: bucket_list.rs shadow_buckets construction (merge filtering only).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MergeKey {
    /// Whether tombstone entries are kept (level < 10).
    pub keep_tombstones: DeadEntryPolicy,
    /// Hash of the curr bucket.
    pub curr_hash: Hash256,
    /// Hash of the snap bucket.
    pub snap_hash: Hash256,
}

impl MergeKey {
    /// Create a new merge key.
    pub fn new(keep_tombstones: DeadEntryPolicy, curr_hash: Hash256, snap_hash: Hash256) -> Self {
        Self {
            keep_tombstones,
            curr_hash,
            snap_hash,
        }
    }
}

/// Serializable representation of a FutureBucket for HistoryArchiveState.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FutureBucketSnapshot {
    /// Current state.
    pub state: FutureBucketState,
    /// Curr bucket hash (when state is HashInputs or LiveInputs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub curr: Option<String>,
    /// Snap bucket hash (when state is HashInputs or LiveInputs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snap: Option<String>,
    /// Output bucket hash (when state is HashOutput or LiveOutput).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

impl Default for FutureBucketSnapshot {
    fn default() -> Self {
        Self {
            state: FutureBucketState::Clear,
            curr: None,
            snap: None,
            output: None,
        }
    }
}

/// State of a merge handle's result receiver.
///
/// Uses an enum to make the invalid "consumed" state (receiver gone, no result)
/// unrepresentable. Transitions from Pending → Ready on first resolution attempt.
enum MergeHandleState {
    /// Merge in progress; receiver delivers the result.
    Pending(oneshot::Receiver<Result<Bucket>>),
    /// Terminal state — merge completed or failed; result is cached.
    /// Errors are stored as strings so the state can be returned multiple times
    /// (BucketError contains non-Clone variants like io::Error).
    Ready(std::result::Result<Bucket, String>),
}

/// Async handle to receive the result of a background merge.
pub struct MergeHandle {
    state: MergeHandleState,
}

impl MergeHandle {
    /// Create a new MergeHandle from a oneshot receiver.
    fn new(receiver: oneshot::Receiver<Result<Bucket>>) -> Self {
        Self {
            state: MergeHandleState::Pending(receiver),
        }
    }

    /// Check if the merge is complete without blocking.
    ///
    /// If the result is ready, it is cached internally so that a subsequent
    /// resolve() call can still retrieve it.
    pub fn is_complete(&mut self) -> bool {
        match &mut self.state {
            MergeHandleState::Ready(_) => true,
            MergeHandleState::Pending(receiver) => match receiver.try_recv() {
                Ok(result) => {
                    self.state = match result {
                        Ok(bucket) => MergeHandleState::Ready(Ok(bucket)),
                        Err(e) => MergeHandleState::Ready(Err(e.to_string())),
                    };
                    true
                }
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.state =
                        MergeHandleState::Ready(Err("merge task was cancelled".to_string()));
                    true
                }
                Err(oneshot::error::TryRecvError::Empty) => false,
            },
        }
    }

    /// Wait for the merge to complete and return the result.
    pub async fn resolve(self) -> Result<Bucket> {
        match self.state {
            MergeHandleState::Ready(result) => result.map_err(|msg| BucketError::Merge(msg)),
            MergeHandleState::Pending(receiver) => receiver
                .await
                .map_err(|_| BucketError::Merge("merge task was cancelled".to_string()))?,
        }
    }

    /// Block on the merge result synchronously.
    ///
    /// Uses a runtime-aware blocking strategy:
    /// - Multi-thread runtime: `block_in_place` + `blocking_recv`
    /// - Current-thread runtime: helper thread (avoids `blocking_recv` panic in async context)
    /// - No runtime: `blocking_recv` directly
    ///
    /// Both success and failure are cached — subsequent calls return the same result.
    pub fn resolve_blocking(&mut self) -> Result<Bucket> {
        if let MergeHandleState::Ready(ref result) = self.state {
            return result.clone().map_err(|msg| BucketError::Merge(msg));
        }

        // Take the Pending receiver
        let MergeHandleState::Pending(receiver) = std::mem::replace(
            &mut self.state,
            MergeHandleState::Ready(Err("merge resolve interrupted".to_string())),
        ) else {
            unreachable!("already checked for Ready above");
        };

        let recv_result = blocking_recv_compat(receiver);

        match recv_result {
            Ok(bucket_result) => match bucket_result {
                Ok(bucket) => {
                    self.state = MergeHandleState::Ready(Ok(bucket.clone()));
                    Ok(bucket)
                }
                Err(e) => {
                    let msg = e.to_string();
                    self.state = MergeHandleState::Ready(Err(msg.clone()));
                    Err(BucketError::Merge(msg))
                }
            },
            Err(e) => {
                let msg = e.to_string();
                self.state = MergeHandleState::Ready(Err(msg.clone()));
                Err(e)
            }
        }
    }
}

/// Receive from a tokio oneshot channel using the appropriate blocking strategy
/// for the current runtime context.
///
/// - Multi-thread runtime: `block_in_place(|| receiver.blocking_recv())`
/// - Current-thread runtime: spawns a helper thread (blocking_recv panics in async context)
/// - No runtime: `receiver.blocking_recv()` directly
fn blocking_recv_compat(
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
                // so we hop to a helper thread.
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

/// Private inner state enum — each variant carries only its valid fields,
/// making invalid states unrepresentable at compile time.
///
/// Spec: BUCKETLISTDB_SPEC §7.1 — FutureBucket state validation is now
/// enforced structurally rather than via runtime checks.
enum FutureBucketInner {
    /// No inputs or outputs.
    Clear,

    /// Output hash present; no live output bucket.
    HashOutput { output_hash: Hash256 },

    /// Input hashes present; no live input buckets.
    HashInputs {
        curr_hash: Hash256,
        snap_hash: Hash256,
        keep_tombstones: DeadEntryPolicy,
    },

    /// Live output bucket available (merge completed or loaded).
    LiveOutput {
        output: Arc<Bucket>,
        output_hash: Hash256,
    },

    /// Async merge in progress with live input references.
    LiveMerging {
        curr: Arc<Bucket>,
        snap: Arc<Bucket>,
        curr_hash: Hash256,
        snap_hash: Hash256,
        merge_handle: MergeHandle,
        keep_tombstones: DeadEntryPolicy,
    },
}

/// FutureBucket wraps an async bucket merge operation.
///
/// This enables background merging of buckets while the main thread continues
/// processing. The merge result can be retrieved when ready via `resolve()`.
///
/// Internally uses a state enum to make invalid states unrepresentable at
/// compile time. The public API is preserved for compatibility.
pub struct FutureBucket {
    inner: FutureBucketInner,
}

impl FutureBucket {
    /// Create a new FutureBucket in Clear state.
    pub fn clear() -> Self {
        Self {
            inner: FutureBucketInner::Clear,
        }
    }

    /// Create a new FutureBucket and start a merge operation.
    ///
    /// This immediately starts the merge in a background task.
    pub fn start_merge(
        curr: Arc<Bucket>,
        snap: Arc<Bucket>,
        protocol_version: u32,
        keep_tombstones: DeadEntryPolicy,
        normalize_init: InitEntryPolicy,
    ) -> Self {
        let curr_hash = curr.hash();
        let snap_hash = snap.hash();

        // Create the oneshot channel for the merge result
        let (sender, receiver) = oneshot::channel();

        // Clone the buckets for the merge task
        let curr_clone = curr.clone();
        let snap_clone = snap.clone();

        // Spawn the merge on tokio's blocking thread pool.
        // merge_buckets is CPU-intensive synchronous work — spawn_blocking
        // keeps it off the async executor and ensures the result can be
        // received via blocking_recv on any runtime flavor.
        tokio::task::spawn_blocking(move || {
            let result = merge_buckets(
                &curr_clone,
                &snap_clone,
                &MergeOptions {
                    keep_dead_entries: keep_tombstones,
                    max_protocol_version: protocol_version,
                    normalize_init_entries: normalize_init,
                    ..Default::default()
                },
            );
            // Send the result, ignoring errors if the receiver was dropped
            let _ = sender.send(result);
        });

        Self {
            inner: FutureBucketInner::LiveMerging {
                curr,
                snap,
                curr_hash,
                snap_hash,
                merge_handle: MergeHandle::new(receiver),
                keep_tombstones,
            },
        }
    }

    /// Create a FutureBucket from a completed merge (already has output).
    pub fn from_output(bucket: Arc<Bucket>) -> Self {
        let hash = bucket.hash();
        Self {
            inner: FutureBucketInner::LiveOutput {
                output: bucket,
                output_hash: hash,
            },
        }
    }

    /// Create a FutureBucket from a snapshot (for deserialization).
    pub fn from_snapshot(snapshot: FutureBucketSnapshot) -> Result<Self> {
        match snapshot.state {
            FutureBucketState::Clear => Ok(Self::clear()),
            FutureBucketState::HashOutput => {
                let output_hex = snapshot
                    .output
                    .ok_or_else(|| BucketError::Serialization("missing output hash".to_string()))?;
                let hash = Hash256::from_hex(&output_hex).map_err(|e| {
                    BucketError::Serialization(format!("invalid output hash: {}", e))
                })?;
                Ok(Self {
                    inner: FutureBucketInner::HashOutput { output_hash: hash },
                })
            }
            FutureBucketState::HashInputs => {
                let curr_hex = snapshot
                    .curr
                    .ok_or_else(|| BucketError::Serialization("missing curr hash".to_string()))?;
                let snap_hex = snapshot
                    .snap
                    .ok_or_else(|| BucketError::Serialization("missing snap hash".to_string()))?;
                let curr_hash = Hash256::from_hex(&curr_hex)
                    .map_err(|e| BucketError::Serialization(format!("invalid curr hash: {}", e)))?;
                let snap_hash = Hash256::from_hex(&snap_hex)
                    .map_err(|e| BucketError::Serialization(format!("invalid snap hash: {}", e)))?;
                Ok(Self {
                    inner: FutureBucketInner::HashInputs {
                        curr_hash,
                        snap_hash,
                        // Default to Keep — will be overridden by make_live()
                        keep_tombstones: DeadEntryPolicy::Keep,
                    },
                })
            }
            _ => Err(BucketError::Serialization(format!(
                "invalid deserialized state: {:?}",
                snapshot.state
            ))),
        }
    }

    /// Get the current state.
    pub fn state(&self) -> FutureBucketState {
        match &self.inner {
            FutureBucketInner::Clear => FutureBucketState::Clear,
            FutureBucketInner::HashOutput { .. } => FutureBucketState::HashOutput,
            FutureBucketInner::HashInputs { .. } => FutureBucketState::HashInputs,
            FutureBucketInner::LiveOutput { .. } => FutureBucketState::LiveOutput,
            FutureBucketInner::LiveMerging { .. } => FutureBucketState::LiveInputs,
        }
    }

    /// Validate internal field invariants for the current state.
    ///
    /// With the enum-based design, invalid states are unrepresentable at compile
    /// time. This method is retained for API compatibility but always returns Ok.
    ///
    /// Spec: BUCKETLISTDB_SPEC §7.1 — FutureBucket state validation.
    #[deprecated(note = "Invariants are now enforced at compile time by the enum design")]
    pub fn check_state(&self) -> Result<()> {
        // Hash consistency assertions in debug builds
        debug_assert!(
            {
                match &self.inner {
                    FutureBucketInner::LiveOutput {
                        output,
                        output_hash,
                    } => output.hash() == *output_hash,
                    FutureBucketInner::LiveMerging {
                        curr,
                        snap,
                        curr_hash,
                        snap_hash,
                        ..
                    } => curr.hash() == *curr_hash && snap.hash() == *snap_hash,
                    _ => true,
                }
            },
            "hash/bucket consistency violated"
        );
        Ok(())
    }

    /// Check if this FutureBucket is in a live state (has bucket references).
    pub fn is_live(&self) -> bool {
        matches!(
            &self.inner,
            FutureBucketInner::LiveMerging { .. } | FutureBucketInner::LiveOutput { .. }
        )
    }

    /// Check if a merge is in progress.
    pub fn is_merging(&self) -> bool {
        matches!(&self.inner, FutureBucketInner::LiveMerging { .. })
    }

    /// Check if this FutureBucket is clear (no data).
    pub fn is_clear(&self) -> bool {
        matches!(&self.inner, FutureBucketInner::Clear)
    }

    /// Check if this FutureBucket has hashes but no live references.
    pub fn has_hashes(&self) -> bool {
        matches!(
            &self.inner,
            FutureBucketInner::HashInputs { .. } | FutureBucketInner::HashOutput { .. }
        )
    }

    /// Check if the merge is complete (output is ready).
    ///
    /// Note: For LiveMerging state, use `is_ready()` which takes `&mut self`
    /// to properly check the oneshot channel status.
    pub fn merge_complete(&self) -> bool {
        matches!(&self.inner, FutureBucketInner::LiveOutput { .. })
    }

    /// Check if the merge is ready without blocking.
    pub fn is_ready(&mut self) -> bool {
        match &mut self.inner {
            FutureBucketInner::LiveOutput { .. } => true,
            FutureBucketInner::LiveMerging { merge_handle, .. } => merge_handle.is_complete(),
            _ => false,
        }
    }

    /// Resolve the merge, waiting if necessary.
    ///
    /// After calling this, the FutureBucket will be in LiveOutput state.
    pub async fn resolve(&mut self) -> Result<Arc<Bucket>> {
        match std::mem::replace(&mut self.inner, FutureBucketInner::Clear) {
            FutureBucketInner::LiveOutput {
                output,
                output_hash,
            } => {
                // Put it back
                self.inner = FutureBucketInner::LiveOutput {
                    output: output.clone(),
                    output_hash,
                };
                Ok(output)
            }
            FutureBucketInner::LiveMerging { merge_handle, .. } => {
                // inner is now Clear (fail-closed)
                match merge_handle.resolve().await {
                    Ok(bucket) => {
                        let output = Arc::new(bucket);
                        let output_hash = output.hash();
                        self.inner = FutureBucketInner::LiveOutput {
                            output: output.clone(),
                            output_hash,
                        };
                        Ok(output)
                    }
                    Err(e) => {
                        // inner stays Clear
                        Err(e)
                    }
                }
            }
            other => {
                // Put it back — not a resolvable state
                let state = match &other {
                    FutureBucketInner::Clear => FutureBucketState::Clear,
                    FutureBucketInner::HashOutput { .. } => FutureBucketState::HashOutput,
                    FutureBucketInner::HashInputs { .. } => FutureBucketState::HashInputs,
                    _ => unreachable!(),
                };
                self.inner = other;
                Err(BucketError::Merge(format!(
                    "cannot resolve FutureBucket in state {:?}",
                    state
                )))
            }
        }
    }

    /// Resolve synchronously (blocking).
    ///
    /// If an async merge is in progress (merge_handle is set), blocks on
    /// that task's result rather than re-executing the merge. This matches
    /// stellar-core's `resolve()` which always consumes the future.
    pub fn resolve_blocking(&mut self) -> Result<Arc<Bucket>> {
        match std::mem::replace(&mut self.inner, FutureBucketInner::Clear) {
            FutureBucketInner::LiveOutput {
                output,
                output_hash,
            } => {
                self.inner = FutureBucketInner::LiveOutput {
                    output: output.clone(),
                    output_hash,
                };
                Ok(output)
            }
            FutureBucketInner::LiveMerging {
                mut merge_handle, ..
            } => {
                // inner is now Clear (fail-closed)
                match merge_handle.resolve_blocking() {
                    Ok(bucket) => {
                        let output = Arc::new(bucket);
                        let output_hash = output.hash();
                        self.inner = FutureBucketInner::LiveOutput {
                            output: output.clone(),
                            output_hash,
                        };
                        Ok(output)
                    }
                    Err(e) => {
                        // inner stays Clear — no sync fallback
                        Err(e)
                    }
                }
            }
            other => {
                let state = match &other {
                    FutureBucketInner::Clear => FutureBucketState::Clear,
                    FutureBucketInner::HashOutput { .. } => FutureBucketState::HashOutput,
                    FutureBucketInner::HashInputs { .. } => FutureBucketState::HashInputs,
                    _ => unreachable!(),
                };
                self.inner = other;
                Err(BucketError::Merge(format!(
                    "cannot resolve FutureBucket in state {:?}",
                    state
                )))
            }
        }
    }

    /// Get the output hash if available.
    pub fn output_hash(&self) -> Option<&Hash256> {
        match &self.inner {
            FutureBucketInner::LiveOutput { output_hash, .. } => Some(output_hash),
            FutureBucketInner::HashOutput { output_hash } => Some(output_hash),
            _ => None,
        }
    }

    /// Get the output bucket if available.
    pub fn output(&self) -> Option<&Arc<Bucket>> {
        match &self.inner {
            FutureBucketInner::LiveOutput { output, .. } => Some(output),
            _ => None,
        }
    }

    /// Get all hashes referenced by this FutureBucket.
    pub fn get_hashes(&self) -> Vec<Hash256> {
        match &self.inner {
            FutureBucketInner::Clear => Vec::new(),
            FutureBucketInner::HashOutput { output_hash } => vec![*output_hash],
            FutureBucketInner::HashInputs {
                curr_hash,
                snap_hash,
                ..
            } => {
                vec![*curr_hash, *snap_hash]
            }
            FutureBucketInner::LiveOutput { output_hash, .. } => vec![*output_hash],
            FutureBucketInner::LiveMerging {
                curr_hash,
                snap_hash,
                ..
            } => {
                vec![*curr_hash, *snap_hash]
            }
        }
    }

    /// Create a snapshot for serialization.
    pub fn to_snapshot(&self) -> FutureBucketSnapshot {
        match &self.inner {
            FutureBucketInner::Clear => FutureBucketSnapshot::default(),
            FutureBucketInner::HashOutput { output_hash }
            | FutureBucketInner::LiveOutput { output_hash, .. } => FutureBucketSnapshot {
                state: FutureBucketState::HashOutput,
                curr: None,
                snap: None,
                output: Some(output_hash.to_hex()),
            },
            FutureBucketInner::HashInputs {
                curr_hash,
                snap_hash,
                ..
            }
            | FutureBucketInner::LiveMerging {
                curr_hash,
                snap_hash,
                ..
            } => FutureBucketSnapshot {
                state: FutureBucketState::HashInputs,
                curr: Some(curr_hash.to_hex()),
                snap: Some(snap_hash.to_hex()),
                output: None,
            },
        }
    }

    /// Make this FutureBucket live by loading buckets from the provided loader.
    ///
    /// This is used after deserializing a FutureBucket to restart the merge.
    /// On failure, self remains unchanged (fail-closed).
    pub fn make_live<F>(
        &mut self,
        load_bucket: F,
        protocol_version: u32,
        keep_tombstones: DeadEntryPolicy,
        normalize_init: InitEntryPolicy,
    ) -> Result<()>
    where
        F: Fn(&Hash256) -> Result<Bucket>,
    {
        match &self.inner {
            FutureBucketInner::HashOutput { output_hash } => {
                let hash = *output_hash;
                // All fallible work before mutation
                let bucket = load_and_verify(&hash, &load_bucket)?;
                self.inner = FutureBucketInner::LiveOutput {
                    output: Arc::new(bucket),
                    output_hash: hash,
                };
                Ok(())
            }
            FutureBucketInner::HashInputs {
                curr_hash,
                snap_hash,
                ..
            } => {
                let ch = *curr_hash;
                let sh = *snap_hash;
                // All fallible work before mutation
                let curr = Arc::new(load_and_verify(&ch, &load_bucket)?);
                let snap = Arc::new(load_and_verify(&sh, &load_bucket)?);

                // Start the merge
                let (sender, receiver) = oneshot::channel();
                let curr_clone = curr.clone();
                let snap_clone = snap.clone();

                tokio::task::spawn_blocking(move || {
                    let result = merge_buckets(
                        &curr_clone,
                        &snap_clone,
                        &MergeOptions {
                            keep_dead_entries: keep_tombstones,
                            max_protocol_version: protocol_version,
                            normalize_init_entries: normalize_init,
                            ..Default::default()
                        },
                    );
                    let _ = sender.send(result);
                });

                self.inner = FutureBucketInner::LiveMerging {
                    curr,
                    snap,
                    curr_hash: ch,
                    snap_hash: sh,
                    merge_handle: MergeHandle::new(receiver),
                    keep_tombstones,
                };
                Ok(())
            }
            FutureBucketInner::Clear => Ok(()), // Nothing to do
            _ => Err(BucketError::Merge(format!(
                "cannot make live in state {:?}",
                self.state()
            ))),
        }
    }

    /// Get the merge key for this FutureBucket (for deduplication).
    pub fn merge_key(&self) -> Option<MergeKey> {
        match &self.inner {
            FutureBucketInner::LiveMerging {
                curr_hash,
                snap_hash,
                keep_tombstones,
                ..
            } => Some(MergeKey::new(*keep_tombstones, *curr_hash, *snap_hash)),
            FutureBucketInner::HashInputs {
                curr_hash,
                snap_hash,
                keep_tombstones,
            } => Some(MergeKey::new(*keep_tombstones, *curr_hash, *snap_hash)),
            _ => None,
        }
    }
}

impl Default for FutureBucket {
    fn default() -> Self {
        Self::clear()
    }
}

impl std::fmt::Debug for FutureBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            FutureBucketInner::Clear => f
                .debug_struct("FutureBucket")
                .field("state", &"Clear")
                .finish(),
            FutureBucketInner::HashOutput { output_hash } => f
                .debug_struct("FutureBucket")
                .field("state", &"HashOutput")
                .field("output_hash", &output_hash.to_hex())
                .finish(),
            FutureBucketInner::HashInputs {
                curr_hash,
                snap_hash,
                ..
            } => f
                .debug_struct("FutureBucket")
                .field("state", &"HashInputs")
                .field("curr_hash", &curr_hash.to_hex())
                .field("snap_hash", &snap_hash.to_hex())
                .finish(),
            FutureBucketInner::LiveOutput { output_hash, .. } => f
                .debug_struct("FutureBucket")
                .field("state", &"LiveOutput")
                .field("output_hash", &output_hash.to_hex())
                .finish(),
            FutureBucketInner::LiveMerging {
                curr_hash,
                snap_hash,
                ..
            } => f
                .debug_struct("FutureBucket")
                .field("state", &"LiveMerging")
                .field("curr_hash", &curr_hash.to_hex())
                .field("snap_hash", &snap_hash.to_hex())
                .finish(),
        }
    }
}

#[cfg(test)]
impl FutureBucket {
    /// Test-only constructor for creating a FutureBucket in LiveMerging state
    /// with a pre-built merge handle (bypasses start_merge's spawn_blocking).
    fn from_test_merge_handle(
        curr: Arc<Bucket>,
        snap: Arc<Bucket>,
        merge_handle: MergeHandle,
    ) -> Self {
        FutureBucket {
            inner: FutureBucketInner::LiveMerging {
                curr_hash: curr.hash(),
                snap_hash: snap.hash(),
                curr,
                snap,
                merge_handle,
                keep_tombstones: DeadEntryPolicy::Keep,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::BucketEntry;
    use stellar_xdr::curr::*;

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

    #[test]
    fn test_future_bucket_clear() {
        let fb = FutureBucket::clear();
        assert!(fb.is_clear());
        assert!(!fb.is_live());
        assert!(!fb.is_merging());
        assert!(!fb.has_hashes());
    }

    #[test]
    fn test_future_bucket_from_output() {
        let entry = make_account_entry([1u8; 32], 100);
        let bucket = Bucket::from_entries(vec![BucketEntry::Liveentry(entry)]).unwrap();
        let hash = bucket.hash();
        let bucket = Arc::new(bucket);

        let fb = FutureBucket::from_output(bucket.clone());
        assert!(!fb.is_clear());
        assert!(fb.is_live());
        assert!(!fb.is_merging());
        assert_eq!(fb.output_hash(), Some(&hash));
        assert!(fb.output().is_some());
    }

    #[test]
    fn test_future_bucket_snapshot_roundtrip() {
        // Test Clear state
        let fb = FutureBucket::clear();
        let snapshot = fb.to_snapshot();
        let fb2 = FutureBucket::from_snapshot(snapshot).unwrap();
        assert!(fb2.is_clear());

        // Test HashOutput state
        let entry = make_account_entry([1u8; 32], 100);
        let bucket = Bucket::from_entries(vec![BucketEntry::Liveentry(entry)]).unwrap();
        let bucket = Arc::new(bucket);
        let fb = FutureBucket::from_output(bucket);
        let snapshot = fb.to_snapshot();
        assert_eq!(snapshot.state, FutureBucketState::HashOutput);
        assert!(snapshot.output.is_some());

        let fb2 = FutureBucket::from_snapshot(snapshot).unwrap();
        assert_eq!(fb2.state(), FutureBucketState::HashOutput);
        assert!(fb2.output_hash().is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_future_bucket_resolve_blocking() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);

        let bucket1 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap());
        let bucket2 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry2)]).unwrap());

        // Use start_merge which spawns a real merge task
        let mut fb = FutureBucket::start_merge(
            bucket1,
            bucket2,
            25,
            DeadEntryPolicy::Keep,
            InitEntryPolicy::Preserve,
        );

        let result = fb.resolve_blocking().unwrap();
        assert!(fb.is_live());
        assert!(!fb.is_merging());
        assert_eq!(fb.state(), FutureBucketState::LiveOutput);
        assert!(fb.output().is_some());
        // Result contains: metadata entry + 2 account entries = 3 entries
        assert!(result.len() >= 2); // At least both account entries merged
    }

    #[test]
    fn test_merge_key() {
        let hash1 = Hash256::from_bytes([1u8; 32]);
        let hash2 = Hash256::from_bytes([2u8; 32]);

        let key = MergeKey::new(DeadEntryPolicy::Keep, hash1, hash2);
        assert_eq!(key.keep_tombstones, DeadEntryPolicy::Keep);
        assert_eq!(key.curr_hash, hash1);
        assert_eq!(key.snap_hash, hash2);
    }

    #[test]
    fn test_get_hashes() {
        let entry = make_account_entry([1u8; 32], 100);
        let bucket = Bucket::from_entries(vec![BucketEntry::Liveentry(entry)]).unwrap();
        let bucket = Arc::new(bucket);

        let fb = FutureBucket::from_output(bucket);
        let hashes = fb.get_hashes();
        assert_eq!(hashes.len(), 1);
    }

    // ============ P3-1: Merge Reattach to Running Merge ============
    //
    // stellar-core: BucketManagerTests.cpp "bucketmanager reattach to running merge"
    // Tests that an in-progress merge can be serialized (snapshot),
    // deserialized, and restarted via make_live(), producing identical results.

    #[tokio::test(flavor = "multi_thread")]
    async fn test_reattach_to_running_merge() {
        // Create two buckets with distinct entries
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);
        let entry3 = make_account_entry([3u8; 32], 300);

        let bucket1 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap());
        let bucket2 = Arc::new(
            Bucket::from_entries(vec![
                BucketEntry::Liveentry(entry2),
                BucketEntry::Liveentry(entry3),
            ])
            .unwrap(),
        );

        let b1_hash = bucket1.hash();
        let b2_hash = bucket2.hash();

        // Start a merge (enters LiveMerging state)
        let mut fb = FutureBucket::start_merge(
            bucket1.clone(),
            bucket2.clone(),
            25,
            DeadEntryPolicy::Keep,
            InitEntryPolicy::Preserve,
        );
        assert_eq!(fb.state(), FutureBucketState::LiveInputs);

        // Resolve the original merge to get the expected result
        let original_result = fb.resolve().await.unwrap();
        let original_hash = original_result.hash();
        let original_len = original_result.len();

        // Now simulate a restart: serialize the in-progress state
        // We manually create a HashInputs snapshot (as if we captured it mid-merge)
        let snapshot = FutureBucketSnapshot {
            state: FutureBucketState::HashInputs,
            curr: Some(b1_hash.to_hex()),
            snap: Some(b2_hash.to_hex()),
            output: None,
        };

        // Deserialize from snapshot
        let mut fb2 = FutureBucket::from_snapshot(snapshot).unwrap();
        assert_eq!(fb2.state(), FutureBucketState::HashInputs);
        assert!(!fb2.is_live());

        // Make live - this restarts the merge by loading buckets
        let b1_clone = bucket1.clone();
        let b2_clone = bucket2.clone();
        fb2.make_live(
            |hash| {
                if *hash == b1_hash {
                    Ok((*b1_clone).clone())
                } else if *hash == b2_hash {
                    Ok((*b2_clone).clone())
                } else {
                    Err(BucketError::Merge("bucket not found".to_string()))
                }
            },
            25,
            DeadEntryPolicy::Keep,
            InitEntryPolicy::Preserve,
        )
        .unwrap();

        assert_eq!(fb2.state(), FutureBucketState::LiveInputs);
        assert!(fb2.is_live());
        assert!(fb2.is_merging());

        // Resolve the restarted merge
        let reattached_result = fb2.resolve().await.unwrap();
        assert_eq!(fb2.state(), FutureBucketState::LiveOutput);

        // Results should be identical
        assert_eq!(reattached_result.hash(), original_hash);
        assert_eq!(reattached_result.len(), original_len);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_reattach_to_finished_merge() {
        // Start and complete a merge
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);

        let bucket1 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap());
        let bucket2 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry2)]).unwrap());

        let mut fb = FutureBucket::start_merge(
            bucket1.clone(),
            bucket2.clone(),
            25,
            DeadEntryPolicy::Keep,
            InitEntryPolicy::Preserve,
        );
        let result = fb.resolve().await.unwrap();
        let result_hash = result.hash();

        // Serialize - should be HashOutput (merge is done)
        let snapshot = fb.to_snapshot();
        assert_eq!(snapshot.state, FutureBucketState::HashOutput);
        assert!(snapshot.output.is_some());

        // Deserialize
        let mut fb2 = FutureBucket::from_snapshot(snapshot).unwrap();
        assert_eq!(fb2.state(), FutureBucketState::HashOutput);

        // Make live by loading the output bucket
        let result_clone = result.clone();
        fb2.make_live(
            |hash| {
                if *hash == result_hash {
                    Ok((*result_clone).clone())
                } else {
                    Err(BucketError::Merge("bucket not found".to_string()))
                }
            },
            25,
            DeadEntryPolicy::Keep,
            InitEntryPolicy::Preserve,
        )
        .unwrap();

        assert_eq!(fb2.state(), FutureBucketState::LiveOutput);
        assert!(fb2.output().is_some());
        assert_eq!(fb2.output().unwrap().hash(), result_hash);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_snapshot_roundtrip_all_states() {
        // Clear state
        let fb = FutureBucket::clear();
        let snap = fb.to_snapshot();
        let fb2 = FutureBucket::from_snapshot(snap).unwrap();
        assert!(fb2.is_clear());

        // HashOutput state
        let entry = make_account_entry([1u8; 32], 100);
        let bucket = Bucket::from_entries(vec![BucketEntry::Liveentry(entry)]).unwrap();
        let bucket = Arc::new(bucket);
        let fb = FutureBucket::from_output(bucket);
        let snap = fb.to_snapshot();
        assert_eq!(snap.state, FutureBucketState::HashOutput);
        let fb2 = FutureBucket::from_snapshot(snap).unwrap();
        assert_eq!(fb2.state(), FutureBucketState::HashOutput);

        // LiveMerging state -> serializes as HashInputs
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);
        let b1 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap());
        let b2 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry2)]).unwrap());
        let fb =
            FutureBucket::start_merge(b1, b2, 25, DeadEntryPolicy::Keep, InitEntryPolicy::Preserve);
        let snap = fb.to_snapshot();
        assert_eq!(snap.state, FutureBucketState::HashInputs);
        assert!(snap.curr.is_some());
        assert!(snap.snap.is_some());
        let fb2 = FutureBucket::from_snapshot(snap).unwrap();
        assert_eq!(fb2.state(), FutureBucketState::HashInputs);
    }

    // ============ P3-2: Bucket Persistence Across Restart ============
    //
    // stellar-core: BucketManagerTests.cpp "bucket persistence over app restart"

    #[tokio::test(flavor = "multi_thread")]
    async fn test_persistence_across_simulated_restart() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);
        let entry3 = make_account_entry([3u8; 32], 300);

        let bucket1 =
            Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry1.clone())]).unwrap());
        let bucket2 = Arc::new(
            Bucket::from_entries(vec![
                BucketEntry::Liveentry(entry2.clone()),
                BucketEntry::Liveentry(entry3.clone()),
            ])
            .unwrap(),
        );

        // Use start_merge then resolve_blocking for synchronous merge
        let mut fb = FutureBucket::start_merge(
            bucket1.clone(),
            bucket2.clone(),
            25,
            DeadEntryPolicy::Keep,
            InitEntryPolicy::Preserve,
        );

        let merged = fb.resolve_blocking().unwrap();
        let merged_hash = merged.hash();
        let merged_len = merged.len();

        // Serialize the completed merge state
        let snapshot = fb.to_snapshot();
        assert_eq!(snapshot.state, FutureBucketState::HashOutput);

        // Simulate full restart: drop everything, recreate from snapshot
        drop(fb);
        drop(merged);

        let mut restored = FutureBucket::from_snapshot(snapshot).unwrap();
        assert_eq!(restored.state(), FutureBucketState::HashOutput);

        // Recreate the output bucket (simulating disk reload)
        let entry1_new = make_account_entry([1u8; 32], 100);
        let entry2_new = make_account_entry([2u8; 32], 200);
        let entry3_new = make_account_entry([3u8; 32], 300);

        let b1_new = Bucket::from_entries(vec![BucketEntry::Liveentry(entry1_new)]).unwrap();
        let b2_new = Bucket::from_entries(vec![
            BucketEntry::Liveentry(entry2_new),
            BucketEntry::Liveentry(entry3_new),
        ])
        .unwrap();

        // Re-merge to get the output bucket
        let re_merged = crate::merge::merge_buckets(
            &b1_new,
            &b2_new,
            &crate::merge::MergeOptions {
                keep_dead_entries: DeadEntryPolicy::Keep,
                max_protocol_version: 25,
                normalize_init_entries: InitEntryPolicy::Preserve,
                ..Default::default()
            },
        )
        .unwrap();

        // Verify re-merged has same hash as original
        assert_eq!(re_merged.hash(), merged_hash);
        assert_eq!(re_merged.len(), merged_len);

        // Make live using the re-merged bucket
        restored
            .make_live(
                |hash| {
                    if *hash == merged_hash {
                        Ok(re_merged.clone())
                    } else {
                        Err(BucketError::Merge("bucket not found".to_string()))
                    }
                },
                25,
                DeadEntryPolicy::Keep,
                InitEntryPolicy::Preserve,
            )
            .unwrap();

        assert_eq!(restored.state(), FutureBucketState::LiveOutput);
        assert_eq!(restored.output().unwrap().hash(), merged_hash);
        assert_eq!(restored.output().unwrap().len(), merged_len);
    }

    #[test]
    fn test_persistence_with_incomplete_merge() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);

        let bucket1 = Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap();
        let bucket2 = Bucket::from_entries(vec![BucketEntry::Liveentry(entry2)]).unwrap();

        let b1_hash = bucket1.hash();
        let b2_hash = bucket2.hash();

        // Create a snapshot as if we captured it mid-merge
        let snapshot = FutureBucketSnapshot {
            state: FutureBucketState::HashInputs,
            curr: Some(b1_hash.to_hex()),
            snap: Some(b2_hash.to_hex()),
            output: None,
        };

        // Deserialize - we get HashInputs, not LiveInputs
        let restored = FutureBucket::from_snapshot(snapshot).unwrap();
        assert_eq!(restored.state(), FutureBucketState::HashInputs);
        assert!(!restored.is_live());
        assert!(!restored.is_merging());

        // Verify hashes are preserved
        let hashes = restored.get_hashes();
        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains(&b1_hash));
        assert!(hashes.contains(&b2_hash));
    }

    #[test]
    fn test_snapshot_hash_preservation() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);
        let bucket1 = Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap();
        let bucket2 = Bucket::from_entries(vec![BucketEntry::Liveentry(entry2)]).unwrap();
        let hash1 = bucket1.hash();
        let hash2 = bucket2.hash();

        // HashOutput snapshot preserves output hash
        let fb = FutureBucket::from_output(Arc::new(bucket1));
        let snap = fb.to_snapshot();
        let restored = FutureBucket::from_snapshot(snap).unwrap();
        assert_eq!(restored.output_hash(), Some(&hash1));

        // HashInputs snapshot preserves input hashes
        let snap = FutureBucketSnapshot {
            state: FutureBucketState::HashInputs,
            curr: Some(hash1.to_hex()),
            snap: Some(hash2.to_hex()),
            output: None,
        };
        let restored = FutureBucket::from_snapshot(snap).unwrap();
        let hashes = restored.get_hashes();
        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains(&hash1));
        assert!(hashes.contains(&hash2));
    }

    /// [AUDIT-BH3] is_complete/is_ready must not consume the merge result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_audit_bh3_is_complete_does_not_consume_result() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);

        let bucket1 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap());
        let bucket2 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry2)]).unwrap());

        let mut fb = FutureBucket::start_merge(
            bucket1,
            bucket2,
            25,
            DeadEntryPolicy::Keep,
            InitEntryPolicy::Preserve,
        );

        // Wait for the merge to complete
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Call is_ready() which calls is_complete() — this should NOT consume the result
        assert!(fb.is_ready(), "merge should be complete after waiting");

        // Now resolve() should still succeed
        let result = fb.resolve().await;
        assert!(
            result.is_ok(),
            "resolve() should succeed after is_ready(): {:?}",
            result.err()
        );
    }

    /// [AUDIT-BH2] resolve_blocking must consume the in-flight async merge
    /// rather than re-executing the merge synchronously.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_audit_bh2_resolve_blocking_consumes_async_merge() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);

        let bucket1 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap());
        let bucket2 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry2)]).unwrap());

        // Start a real merge
        let mut fb = FutureBucket::start_merge(
            bucket1,
            bucket2,
            25,
            DeadEntryPolicy::Keep,
            InitEntryPolicy::Preserve,
        );

        // resolve_blocking should block on the async merge handle
        let result = fb.resolve_blocking();
        assert!(
            result.is_ok(),
            "resolve_blocking should consume the merge_handle: {:?}",
            result.err()
        );
        assert_eq!(fb.state(), FutureBucketState::LiveOutput);
    }

    #[test]
    fn test_make_live_hash_output_mismatch() {
        let entry = make_account_entry([1u8; 32], 100);
        let bucket = Bucket::from_entries(vec![BucketEntry::Liveentry(entry.clone())]).unwrap();
        let expected_hash = bucket.hash();

        let snapshot = FutureBucketSnapshot {
            state: FutureBucketState::HashOutput,
            curr: None,
            snap: None,
            output: Some(expected_hash.to_hex()),
        };
        let mut fb = FutureBucket::from_snapshot(snapshot).unwrap();
        assert_eq!(fb.state(), FutureBucketState::HashOutput);

        // Loader returns a bucket with a different hash
        let wrong_entry = make_account_entry([2u8; 32], 200);
        let wrong_bucket = Bucket::from_entries(vec![BucketEntry::Liveentry(wrong_entry)]).unwrap();
        assert_ne!(wrong_bucket.hash(), expected_hash);

        let result = fb.make_live(
            |_| Ok(wrong_bucket.clone()),
            25,
            DeadEntryPolicy::Keep,
            InitEntryPolicy::Preserve,
        );

        assert!(matches!(result, Err(BucketError::HashMismatch { .. })));
        // Verify no partial state transition
        assert_eq!(fb.state(), FutureBucketState::HashOutput);
        assert!(!fb.is_live());
        assert!(!fb.is_merging());
        assert!(fb.output().is_none());
    }

    #[test]
    fn test_make_live_hash_inputs_curr_mismatch() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);
        let bucket1 = Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap();
        let bucket2 = Bucket::from_entries(vec![BucketEntry::Liveentry(entry2)]).unwrap();
        let h1 = bucket1.hash();
        let h2 = bucket2.hash();

        let snapshot = FutureBucketSnapshot {
            state: FutureBucketState::HashInputs,
            curr: Some(h1.to_hex()),
            snap: Some(h2.to_hex()),
            output: None,
        };
        let mut fb = FutureBucket::from_snapshot(snapshot).unwrap();
        assert_eq!(fb.state(), FutureBucketState::HashInputs);

        // Loader returns wrong bucket for curr (swaps them)
        let b2_clone = bucket2.clone();
        let result = fb.make_live(
            |hash| {
                if *hash == h1 {
                    Ok(b2_clone.clone())
                } else {
                    Ok(bucket2.clone())
                }
            },
            25,
            DeadEntryPolicy::Keep,
            InitEntryPolicy::Preserve,
        );

        assert!(matches!(result, Err(BucketError::HashMismatch { .. })));
        assert_eq!(fb.state(), FutureBucketState::HashInputs);
        assert!(!fb.is_live());
        assert!(!fb.is_merging());
    }

    #[test]
    fn test_make_live_hash_inputs_snap_mismatch() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);
        let bucket1 = Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap();
        let bucket2 = Bucket::from_entries(vec![BucketEntry::Liveentry(entry2)]).unwrap();
        let h1 = bucket1.hash();
        let h2 = bucket2.hash();

        let snapshot = FutureBucketSnapshot {
            state: FutureBucketState::HashInputs,
            curr: Some(h1.to_hex()),
            snap: Some(h2.to_hex()),
            output: None,
        };
        let mut fb = FutureBucket::from_snapshot(snapshot).unwrap();
        assert_eq!(fb.state(), FutureBucketState::HashInputs);

        // Loader returns correct curr but wrong snap
        let b1_clone = bucket1.clone();
        let result = fb.make_live(
            |hash| {
                if *hash == h1 {
                    Ok(b1_clone.clone())
                } else {
                    Ok(b1_clone.clone())
                }
            },
            25,
            DeadEntryPolicy::Keep,
            InitEntryPolicy::Preserve,
        );

        assert!(matches!(result, Err(BucketError::HashMismatch { .. })));
        assert_eq!(fb.state(), FutureBucketState::HashInputs);
        assert!(!fb.is_live());
        assert!(!fb.is_merging());
    }

    /// Regression test for issue #2498 defect A: MergeHandle::resolve_blocking()
    /// must work on a current_thread tokio runtime without panicking.
    #[tokio::test(flavor = "current_thread")]
    async fn test_merge_handle_resolve_blocking_on_current_thread_runtime() {
        let (sender, receiver) = oneshot::channel();

        let entry = make_account_entry([42u8; 32], 500);
        let bucket = Bucket::from_entries(vec![BucketEntry::Liveentry(entry)]).unwrap();
        let expected_hash = bucket.hash();

        sender.send(Ok(bucket)).unwrap();

        let mut handle = MergeHandle::new(receiver);
        let result = handle.resolve_blocking();
        assert!(
            result.is_ok(),
            "resolve_blocking must not panic on current_thread"
        );
        assert_eq!(result.unwrap().hash(), expected_hash);
    }

    /// Regression test for issue #2498 defect B: resolve_blocking() after the
    /// sender is dropped (cancelled) must return an error and cache it.
    #[tokio::test(flavor = "current_thread")]
    async fn test_merge_handle_resolve_blocking_after_cancel() {
        let (sender, receiver) = oneshot::channel::<Result<Bucket>>();
        drop(sender);

        let mut handle = MergeHandle::new(receiver);

        let result = handle.resolve_blocking();
        assert!(result.is_err(), "cancelled merge should return error");

        // Second call returns the cached error
        let result2 = handle.resolve_blocking();
        assert!(result2.is_err(), "cached error should persist");
    }

    /// Regression test for issue #2498: FutureBucket::resolve_blocking() must
    /// work on current_thread runtime when an async merge is in progress.
    #[tokio::test(flavor = "current_thread")]
    async fn test_future_bucket_resolve_blocking_on_current_thread_runtime() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);
        let bucket1 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap());
        let bucket2 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry2)]).unwrap());

        let (sender, receiver) = oneshot::channel();
        let merged = merge_buckets(
            &bucket1,
            &bucket2,
            &MergeOptions {
                keep_dead_entries: DeadEntryPolicy::Keep,
                max_protocol_version: 25,
                normalize_init_entries: InitEntryPolicy::Preserve,
                ..Default::default()
            },
        )
        .unwrap();
        let expected_hash = merged.hash();

        sender.send(Ok(merged)).unwrap();

        let mut fb =
            FutureBucket::from_test_merge_handle(bucket1, bucket2, MergeHandle::new(receiver));

        let result = fb.resolve_blocking();
        assert!(
            result.is_ok(),
            "resolve_blocking must not panic on current_thread"
        );
        assert_eq!(result.unwrap().hash(), expected_hash);
        assert_eq!(fb.state(), FutureBucketState::LiveOutput);
    }

    /// Regression test for issue #2498 defect B: FutureBucket::resolve_blocking()
    /// must transition to Clear on failure, not leave a dead-end state.
    #[tokio::test(flavor = "current_thread")]
    async fn test_future_bucket_resolve_blocking_error_transitions_to_clear() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);
        let bucket1 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap());
        let bucket2 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry2)]).unwrap());

        let (sender, receiver) = oneshot::channel();
        // Drop sender to simulate cancelled merge
        drop(sender);

        let mut fb =
            FutureBucket::from_test_merge_handle(bucket1, bucket2, MergeHandle::new(receiver));

        let result = fb.resolve_blocking();
        assert!(result.is_err(), "cancelled merge should fail");
        assert_eq!(
            fb.state(),
            FutureBucketState::Clear,
            "failed resolve_blocking must transition to Clear"
        );
    }

    // ========================================================================
    // State introspection tests
    // ========================================================================

    #[tokio::test(flavor = "multi_thread")]
    async fn test_state_returns_live_inputs_for_merging() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);
        let bucket1 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap());
        let bucket2 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry2)]).unwrap());

        let fb = FutureBucket::start_merge(
            bucket1,
            bucket2,
            25,
            DeadEntryPolicy::Keep,
            InitEntryPolicy::Preserve,
        );

        // External API sees LiveInputs for the LiveMerging internal state
        assert_eq!(fb.state(), FutureBucketState::LiveInputs);
        assert!(fb.is_merging());
        assert!(fb.is_live());
    }

    #[test]
    #[allow(deprecated)]
    fn test_check_state_always_ok() {
        let fb = FutureBucket::clear();
        assert!(fb.check_state().is_ok());

        let entry = make_account_entry([1u8; 32], 100);
        let bucket = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry)]).unwrap());
        let fb = FutureBucket::from_output(bucket);
        assert!(fb.check_state().is_ok());
    }

    #[test]
    fn test_merge_key_from_hash_inputs() {
        let snapshot = FutureBucketSnapshot {
            state: FutureBucketState::HashInputs,
            curr: Some(Hash256::from_bytes([1u8; 32]).to_hex()),
            snap: Some(Hash256::from_bytes([2u8; 32]).to_hex()),
            output: None,
        };
        let fb = FutureBucket::from_snapshot(snapshot).unwrap();
        let key = fb.merge_key();
        assert!(key.is_some());
        let key = key.unwrap();
        assert_eq!(key.curr_hash, Hash256::from_bytes([1u8; 32]));
        assert_eq!(key.snap_hash, Hash256::from_bytes([2u8; 32]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_merge_key_from_live_merging() {
        let entry1 = make_account_entry([1u8; 32], 100);
        let entry2 = make_account_entry([2u8; 32], 200);
        let bucket1 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry1)]).unwrap());
        let bucket2 = Arc::new(Bucket::from_entries(vec![BucketEntry::Liveentry(entry2)]).unwrap());
        let h1 = bucket1.hash();
        let h2 = bucket2.hash();

        let fb = FutureBucket::start_merge(
            bucket1,
            bucket2,
            25,
            DeadEntryPolicy::Keep,
            InitEntryPolicy::Preserve,
        );
        let key = fb.merge_key().unwrap();
        assert_eq!(key.curr_hash, h1);
        assert_eq!(key.snap_hash, h2);
        assert_eq!(key.keep_tombstones, DeadEntryPolicy::Keep);
    }
}
