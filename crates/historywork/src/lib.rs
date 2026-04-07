//! History work items for Stellar Core catchup and publish workflows.
//!
//! This crate provides the building blocks for downloading and publishing Stellar
//! history archive data. It implements a work-item based architecture that integrates
//! with the [`henyey_work`] scheduler to orchestrate complex multi-step operations
//! with proper dependency management and retry logic.
//!
//! # Overview
//!
//! History archives store snapshots of the Stellar ledger at regular checkpoint intervals
//! (every 64 ledgers). This crate provides work items to:
//!
//! - **Download** history data: HAS (History Archive State), buckets, ledger headers,
//!   transactions, transaction results, and SCP consensus history
//! - **Verify** downloaded data: hash verification for buckets, header chain validation,
//!   and transaction set integrity checks
//!
//! # Architecture
//!
//! Work items are organized as a directed acyclic graph (DAG) of dependencies:
//!
//! ```text
//!                    ┌─────────────┐
//!                    │  Fetch HAS  │
//!                    └──────┬──────┘
//!                           │
//!           ┌───────────────┼───────────────┐
//!           │               │               │
//!           ▼               ▼               ▼
//!    ┌─────────────┐ ┌─────────────┐ ┌─────────────┐
//!    │  Download   │ │  Download   │ │  Download   │
//!    │  Buckets    │ │  Headers    │ │    SCP      │
//!    └─────────────┘ └──────┬──────┘ └─────────────┘
//!                           │
//!                    ┌──────┴──────┐
//!                    ▼             ▼
//!             ┌─────────────┐ ┌─────────────┐
//!             │  Download   │ │  Download   │
//!             │Transactions │ │  Results    │
//!             └─────────────┘ └─────────────┘
//! ```
//!
//! All work items share state through [`SharedHistoryState`], a thread-safe container
//! that accumulates downloaded data as work progresses.
//!
//! # Usage
//!
//! ## Downloading checkpoint data
//!
//! Use [`HistoryWorkBuilder`] to register download work items with a scheduler:
//!
//! ```rust,ignore
//! use henyey_historywork::{HistoryWorkBuilder, SharedHistoryState};
//! use henyey_work::{WorkScheduler, WorkSchedulerConfig};
//! use std::path::PathBuf;
//!
//! // Create shared state for work items
//! let state: SharedHistoryState = Default::default();
//!
//! // Build and register work items
//! let builder = HistoryWorkBuilder::new(
//!     archive,
//!     checkpoint,
//!     state.clone(),
//!     PathBuf::from("/tmp/buckets"),
//! );
//! let mut scheduler = WorkScheduler::new(WorkSchedulerConfig::default());
//! let work_ids = builder.register(&mut scheduler);
//!
//! // Run the scheduler to completion
//! scheduler.run_until_done().await;
//!
//! // Extract downloaded data for catchup
//! let checkpoint_data = build_checkpoint_data(&state).await?;
//! ```
//!
//! # Key Types
//!
//! - [`HistoryWorkState`]: Shared container for downloaded history data
//! - [`HistoryWorkBuilder`]: Factory for registering work items with proper dependencies
//! - [`CheckpointData`]: Complete snapshot of a checkpoint for catchup operations

mod builder;
mod download;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::sync::Mutex;

use henyey_history::CheckpointData;
use stellar_xdr::curr::{
    LedgerHeaderHistoryEntry, ScpHistoryEntry, TransactionHistoryEntry,
    TransactionHistoryResultEntry,
};

pub use builder::{HistoryWorkBuilder, HistoryWorkIds};

// ============================================================================
// Shared State Types
// ============================================================================

/// Shared state container for history work items.
///
/// This struct accumulates data as download work items complete. Each field
/// is populated by its corresponding download work item and consumed by
/// either verification steps or the final [`build_checkpoint_data`] call.
///
/// # Thread Safety
///
/// This type is wrapped in [`SharedHistoryState`] (an `Arc<Mutex<...>>`) for
/// safe sharing between concurrent work items. Work items acquire the lock
/// briefly to read dependencies or write their output.
///
/// # Fields
///
/// - `has`: The History Archive State describing the checkpoint's bucket list
/// - `bucket_dir`: Directory where bucket files are stored on disk
/// - `headers`: Ledger headers for all ledgers in the checkpoint range
/// - `transactions`: Transaction sets for each ledger
/// - `tx_results`: Transaction results (meta) for each ledger
/// - `scp_history`: SCP consensus messages for the checkpoint
/// - `progress`: Current work stage and status message for monitoring
#[derive(Debug, Default)]
pub struct HistoryWorkState {
    /// The History Archive State (HAS) for this checkpoint.
    ///
    /// Contains the bucket list structure that describes the complete ledger
    /// state at the checkpoint boundary.
    pub has: Option<henyey_history::archive_state::HistoryArchiveState>,

    /// Directory where downloaded bucket files are stored on disk.
    ///
    /// Buckets are saved as `<hex_hash>.bucket` files during download.
    /// This avoids holding multi-GB bucket data in memory.
    pub bucket_dir: Option<PathBuf>,

    /// Ledger header history entries for the checkpoint range.
    ///
    /// Contains headers for 64 consecutive ledgers, linking each ledger to
    /// its predecessor via the `previous_ledger_hash` field.
    pub headers: Vec<LedgerHeaderHistoryEntry>,

    /// Transaction history entries containing transaction sets.
    ///
    /// Each entry contains all transactions applied in a single ledger,
    /// either as a classic transaction set or a generalized (phase-based) set.
    pub transactions: Vec<TransactionHistoryEntry>,

    /// Transaction result entries containing execution results and metadata.
    ///
    /// Stores the outcome of each transaction including fee charges,
    /// operation results, and ledger changes.
    pub tx_results: Vec<TransactionHistoryResultEntry>,

    /// SCP consensus history for the checkpoint.
    ///
    /// Records the consensus messages exchanged to close each ledger,
    /// useful for auditing and debugging consensus behavior.
    pub scp_history: Vec<ScpHistoryEntry>,

    /// Current progress indicator for monitoring work execution.
    pub progress: HistoryWorkProgress,
}

/// Thread-safe handle to shared history work state.
///
/// This type alias wraps [`HistoryWorkState`] in an `Arc<Mutex<...>>` for
/// safe sharing between work items. Use `state.lock().await` to access
/// the underlying state.
///
/// # Example
///
/// ```rust,ignore
/// let state: SharedHistoryState = Default::default();
///
/// // In a work item:
/// let mut guard = state.lock().await;
/// guard.has = Some(downloaded_has);
/// ```
pub type SharedHistoryState = Arc<Mutex<HistoryWorkState>>;

/// Identifies the current stage of history work execution.
///
/// This enum is used for progress reporting and monitoring. Each variant
/// corresponds to a specific work item in the download or publish pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryWorkStage {
    /// Fetching the History Archive State (HAS) JSON file.
    FetchHas,
    /// Downloading bucket files referenced by the HAS.
    DownloadBuckets,
    /// Downloading ledger header XDR files.
    DownloadHeaders,
    /// Downloading transaction set XDR files.
    DownloadTransactions,
    /// Downloading transaction result XDR files.
    DownloadResults,
    /// Downloading SCP consensus history XDR files.
    DownloadScp,
}

/// Progress indicator for history work execution.
///
/// This struct provides a snapshot of the current work stage and a
/// human-readable status message. Use [`get_progress`] to retrieve
/// the current progress from shared state.
///
/// # Example
///
/// ```rust,ignore
/// let progress = get_progress(&state).await;
/// if let Some(stage) = progress.stage {
///     println!("Stage: {:?}, Status: {}", stage, progress.message);
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct HistoryWorkProgress {
    /// The current work stage, if any work is in progress.
    pub stage: Option<HistoryWorkStage>,
    /// Human-readable status message describing the current operation.
    pub message: String,
}

/// Updates the progress indicator in shared state.
///
/// This is an internal helper used by work items to report their current
/// stage and status message.
async fn set_progress(state: &SharedHistoryState, stage: HistoryWorkStage, message: &str) {
    let mut guard = state.lock().await;
    guard.progress.stage = Some(stage);
    guard.progress.message = message.to_string();
}

// ============================================================================
// Public API: Progress and Checkpoint Data Assembly
// ============================================================================

/// Retrieves the current progress indicator from shared state.
///
/// This function never fails and returns default progress if no work
/// has started yet.
pub async fn get_progress(state: &SharedHistoryState) -> HistoryWorkProgress {
    let guard = state.lock().await;
    guard.progress.clone()
}

/// Builds a complete [`CheckpointData`] snapshot from shared state.
///
/// This is the primary way to extract downloaded data for use in catchup
/// operations. Call this after all download work items have completed.
///
/// # Example
///
/// ```rust,ignore
/// // After scheduler completes all work...
/// let checkpoint_data = build_checkpoint_data(&state).await?;
/// catchup_manager
///     .catchup_to_ledger_with_checkpoint_data(target, checkpoint_data)
///     .await?;
/// ```
///
/// # Errors
///
/// Returns an error if the HAS or bucket directory is not available (other
/// fields may be empty).
///
/// # Ownership
///
/// This function *takes* data out of the shared state rather than cloning it,
/// since checkpoint data is only consumed once at the end of a work chain.
/// Calling it a second time will fail because the HAS has been moved.
pub async fn build_checkpoint_data(state: &SharedHistoryState) -> Result<CheckpointData> {
    let mut guard = state.lock().await;
    let has = guard
        .has
        .take()
        .ok_or_else(|| anyhow!("missing History Archive State"))?;

    Ok(CheckpointData {
        has,
        bucket_dir: guard
            .bucket_dir
            .take()
            .ok_or_else(|| anyhow!("bucket directory not set"))?,
        headers: std::mem::take(&mut guard.headers),
        transactions: std::mem::take(&mut guard.transactions),
        tx_results: std::mem::take(&mut guard.tx_results),
        scp_history: std::mem::take(&mut guard.scp_history),
    })
}

#[cfg(test)]
mod tests {
    use henyey_history::download::{RETRY_A_FEW, RETRY_A_LOT};

    // ── CATCHUP_SPEC §9.1: Retry constants re-exported from download ─

    #[test]
    fn test_retry_a_few_constant() {
        assert_eq!(
            RETRY_A_FEW, 5,
            "RETRY_A_FEW must be 5 (matches stellar-core)"
        );
    }

    #[test]
    fn test_retry_a_lot_constant() {
        assert_eq!(
            RETRY_A_LOT, 32,
            "RETRY_A_LOT must be 32 (matches stellar-core)"
        );
    }
}
