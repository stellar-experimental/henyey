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

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use tokio::sync::Mutex;

use henyey_common::Hash256;
use henyey_history::{
    archive::HistoryArchive,
    archive_state::HistoryArchiveState,
    download::{RETRY_A_FEW, RETRY_A_LOT},
    verify, CheckpointData,
};
use henyey_ledger::TransactionSetVariant;
use henyey_work::{Work, WorkContext, WorkId, WorkOutcome, WorkScheduler};
use stellar_xdr::curr::{
    LedgerHeaderHistoryEntry, ScpHistoryEntry, TransactionHistoryEntry, TransactionHistoryEntryExt,
    TransactionHistoryResultEntry, WriteXdr,
};

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
    pub has: Option<HistoryArchiveState>,

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

/// Work item to fetch the History Archive State (HAS) for a checkpoint.
///
/// The HAS is a JSON document that describes the complete bucket list structure
/// at a checkpoint boundary. It is the starting point for catchup operations,
/// as it lists all bucket hashes needed to reconstruct ledger state.
///
/// This work item must complete before any other download work can proceed,
/// as the HAS is required to know which buckets to download.
///
/// # Dependencies
///
/// None - this is the root of the download work graph.
///
/// # Output
///
/// On success, populates `state.has` with the parsed [`HistoryArchiveState`].
pub(crate) struct GetHistoryArchiveStateWork {
    archive: Arc<HistoryArchive>,
    checkpoint: u32,
    state: SharedHistoryState,
}

#[async_trait]
impl Work for GetHistoryArchiveStateWork {
    fn name(&self) -> &str {
        "get-history-archive-state"
    }

    async fn run(&mut self, _ctx: &WorkContext) -> WorkOutcome {
        set_progress(&self.state, HistoryWorkStage::FetchHas, "fetching HAS").await;
        match self.archive.get_checkpoint_has(self.checkpoint).await {
            Ok(has) => {
                let mut guard = self.state.lock().await;
                guard.has = Some(has);
                WorkOutcome::Success
            }
            Err(err) => WorkOutcome::Failed(format!("failed to fetch HAS: {err}")),
        }
    }
}

/// Work item to download and verify bucket files referenced in the HAS.
///
/// Buckets contain the actual ledger entries (accounts, trustlines, offers,
/// contract data, etc.) organized in a multi-level structure. This work item
/// downloads all unique buckets referenced by the HAS and verifies each
/// bucket's SHA-256 hash.
///
/// # Parallelism
///
/// Downloads are performed concurrently with up to 16 parallel requests,
/// matching the stellar-core `MAX_CONCURRENT_SUBPROCESSES` limit.
///
/// # Dependencies
///
/// Requires [`GetHistoryArchiveStateWork`] to complete first, as the HAS
/// contains the list of bucket hashes to download.
///
/// # Output
///
/// On success, saves bucket files to disk in the configured bucket directory.
pub(crate) struct DownloadBucketsWork {
    archive: Arc<HistoryArchive>,
    state: SharedHistoryState,
    bucket_dir: PathBuf,
}

/// Downloads a single bucket, verifies its hash, and saves it to disk.
async fn download_and_save_bucket(
    archive: &HistoryArchive,
    hash: &Hash256,
    bucket_path: &std::path::Path,
) -> Result<(), String> {
    let data = archive
        .get_bucket(hash)
        .await
        .map_err(|err| format!("failed to download bucket {hash}: {err}"))?;

    verify::verify_bucket_hash(&data, hash)
        .map_err(|err| format!("bucket {hash} hash mismatch: {err}"))?;

    std::fs::write(bucket_path, &data)
        .map_err(|e| format!("failed to save bucket {hash} to disk: {e}"))?;

    Ok(())
}

#[async_trait]
impl Work for DownloadBucketsWork {
    fn name(&self) -> &str {
        "download-buckets"
    }

    async fn run(&mut self, _ctx: &WorkContext) -> WorkOutcome {
        set_progress(
            &self.state,
            HistoryWorkStage::DownloadBuckets,
            "downloading buckets",
        )
        .await;
        let has = {
            let guard = self.state.lock().await;
            guard.has.clone()
        };

        let Some(has) = has else {
            return WorkOutcome::Failed("missing HAS".to_string());
        };

        let hashes = content_bucket_hashes(&has);
        let total = hashes.len();
        let archive = self.archive.clone();
        let bucket_dir = self.bucket_dir.clone();

        // Ensure bucket directory exists
        if let Err(e) = std::fs::create_dir_all(&bucket_dir) {
            return WorkOutcome::Failed(format!("failed to create bucket dir: {e}"));
        }

        // Filter out buckets already on disk
        let to_download: Vec<_> = hashes
            .iter()
            .filter(|hash| {
                let path = bucket_dir.join(format!("{}.bucket.xdr", hash.to_hex()));
                !path.exists()
            })
            .cloned()
            .collect();

        if to_download.is_empty() {
            tracing::info!("All {} buckets already cached on disk", total);
        } else {
            tracing::info!(
                "Downloading {} buckets to disk ({} already cached)",
                to_download.len(),
                total - to_download.len()
            );

            let downloaded_count = std::sync::atomic::AtomicU32::new(0);
            let total_to_download = to_download.len();

            let results: Vec<Result<(), String>> = stream::iter(to_download.into_iter())
                .map(|hash| {
                    let archive = archive.clone();
                    let bucket_dir = bucket_dir.clone();
                    let downloaded_count = &downloaded_count;

                    async move {
                        let path = bucket_dir.join(format!("{}.bucket.xdr", hash.to_hex()));
                        download_and_save_bucket(&archive, &hash, &path).await?;

                        let count =
                            downloaded_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        if count % 5 == 0 || count == total_to_download as u32 {
                            tracing::info!("Downloaded {}/{} buckets", count, total_to_download);
                        }
                        Ok(())
                    }
                })
                .buffer_unordered(MAX_CONCURRENT_DOWNLOADS)
                .collect()
                .await;

            // Check for failures
            for result in results {
                if let Err(err) = result {
                    return WorkOutcome::Failed(err);
                }
            }
        }

        tracing::info!("All {} buckets available on disk", total);

        let mut guard = self.state.lock().await;
        guard.bucket_dir = Some(bucket_dir);
        WorkOutcome::Success
    }
}

/// Work item to download and verify ledger headers for a checkpoint.
///
/// Downloads the ledger header history file for a checkpoint range (64 ledgers)
/// and verifies the header chain integrity by checking that each header's
/// `previous_ledger_hash` matches the hash of the preceding header.
///
/// Ledger headers are essential for:
/// - Verifying transaction set hashes
/// - Verifying transaction result hashes
/// - Establishing the ledger sequence and timing
///
/// # Dependencies
///
/// Requires [`GetHistoryArchiveStateWork`] to complete first.
///
/// # Output
///
/// On success, populates `state.headers` with verified header entries.
pub(crate) struct DownloadLedgerHeadersWork {
    archive: Arc<HistoryArchive>,
    checkpoint: u32,
    state: SharedHistoryState,
}

#[async_trait]
impl Work for DownloadLedgerHeadersWork {
    fn name(&self) -> &str {
        "download-ledger-headers"
    }

    async fn run(&mut self, _ctx: &WorkContext) -> WorkOutcome {
        set_progress(
            &self.state,
            HistoryWorkStage::DownloadHeaders,
            "downloading headers",
        )
        .await;
        let headers = match self.archive.get_ledger_headers(self.checkpoint).await {
            Ok(headers) => headers,
            Err(err) => return WorkOutcome::Failed(format!("failed to download headers: {err}")),
        };

        let header_chain: Vec<_> = headers.iter().map(|entry| entry.header.clone()).collect();
        if let Err(err) = verify::verify_header_chain(&header_chain) {
            return WorkOutcome::Failed(format!("header chain verification failed: {err}"));
        }

        let mut guard = self.state.lock().await;
        guard.headers = headers;
        WorkOutcome::Success
    }
}

/// Work item to download and verify transaction sets for a checkpoint.
///
/// Downloads the transaction history file containing all transactions applied
/// during the checkpoint range. Each transaction set is verified against its
/// corresponding ledger header's `tx_set_result_hash`.
///
/// Transaction sets come in two variants:
/// - Classic: original format with a simple list of transactions
/// - Generalized: phase-based format supporting Soroban transactions
///
/// # Dependencies
///
/// Requires [`DownloadLedgerHeadersWork`] to complete first, as headers are
/// needed to verify transaction set hashes.
///
/// # Output
///
/// On success, populates `state.transactions` with verified transaction entries.
pub(crate) struct DownloadTransactionsWork {
    archive: Arc<HistoryArchive>,
    checkpoint: u32,
    state: SharedHistoryState,
}

#[async_trait]
impl Work for DownloadTransactionsWork {
    fn name(&self) -> &str {
        "download-transactions"
    }

    async fn run(&mut self, _ctx: &WorkContext) -> WorkOutcome {
        set_progress(
            &self.state,
            HistoryWorkStage::DownloadTransactions,
            "downloading transactions",
        )
        .await;
        let entries = match self.archive.get_transactions(self.checkpoint).await {
            Ok(entries) => entries,
            Err(err) => {
                return WorkOutcome::Failed(format!("failed to download transactions: {err}"))
            }
        };

        let headers = {
            let guard = self.state.lock().await;
            guard.headers.clone()
        };
        for entry in &entries {
            let header = match find_header(&headers, entry.ledger_seq, "transaction set") {
                Ok(header) => header,
                Err(err) => return WorkOutcome::Failed(err),
            };
            let tx_set = match &entry.ext {
                TransactionHistoryEntryExt::V0 => {
                    TransactionSetVariant::Classic(entry.tx_set.clone())
                }
                TransactionHistoryEntryExt::V1(set) => {
                    TransactionSetVariant::Generalized(set.clone())
                }
            };
            if let Err(err) = verify::verify_tx_set(&header.header, &tx_set) {
                return WorkOutcome::Failed(format!("tx set hash mismatch: {err}"));
            }
        }

        let mut guard = self.state.lock().await;
        guard.transactions = entries;
        WorkOutcome::Success
    }
}

/// Work item to download and verify transaction results for a checkpoint.
///
/// Downloads the transaction results history file containing the execution
/// outcomes and ledger changes (metadata) for all transactions in the
/// checkpoint range. Each result set is verified against its corresponding
/// ledger header's result hash.
///
/// Transaction results include:
/// - Fee charges and refunds
/// - Operation-level success/failure results
/// - Ledger entry changes (creates, updates, deletes)
/// - Soroban contract execution metadata
///
/// # Dependencies
///
/// Requires both [`DownloadLedgerHeadersWork`] and [`DownloadTransactionsWork`]
/// to complete first.
///
/// # Output
///
/// On success, populates `state.tx_results` with verified result entries.
pub(crate) struct DownloadTxResultsWork {
    archive: Arc<HistoryArchive>,
    checkpoint: u32,
    state: SharedHistoryState,
}

#[async_trait]
impl Work for DownloadTxResultsWork {
    fn name(&self) -> &str {
        "download-tx-results"
    }

    async fn run(&mut self, _ctx: &WorkContext) -> WorkOutcome {
        let headers = {
            let guard = self.state.lock().await;
            guard.headers.clone()
        };

        set_progress(
            &self.state,
            HistoryWorkStage::DownloadResults,
            "downloading transaction results",
        )
        .await;
        let results = match self.archive.get_results(self.checkpoint).await {
            Ok(results) => results,
            Err(err) => {
                return WorkOutcome::Failed(format!("failed to download tx results: {err}"))
            }
        };

        for entry in &results {
            let header = match find_header(&headers, entry.ledger_seq, "tx result set") {
                Ok(header) => header,
                Err(err) => return WorkOutcome::Failed(err),
            };
            let xdr = match entry
                .tx_result_set
                .to_xdr(stellar_xdr::curr::Limits::none())
            {
                Ok(xdr) => xdr,
                Err(err) => {
                    return WorkOutcome::Failed(format!(
                        "failed to serialize tx result set for ledger {}: {err}",
                        entry.ledger_seq
                    ))
                }
            };
            if let Err(err) = verify::verify_tx_result_set(&header.header, &xdr) {
                return WorkOutcome::Failed(format!("tx result set hash mismatch: {err}"));
            }
        }

        let mut guard = self.state.lock().await;
        guard.tx_results = results;
        WorkOutcome::Success
    }
}

/// Work item to download SCP consensus history for a checkpoint.
///
/// Downloads the SCP history file containing the consensus protocol messages
/// exchanged to close each ledger in the checkpoint range. This data is
/// optional for catchup but useful for:
///
/// - Auditing consensus behavior and vote distribution
/// - Debugging network issues or validator performance
/// - Historical analysis of the consensus process
///
/// # Dependencies
///
/// Requires [`DownloadLedgerHeadersWork`] to complete first.
///
/// # Output
///
/// On success, populates `state.scp_history` with SCP entries.
pub(crate) struct DownloadScpHistoryWork {
    archive: Arc<HistoryArchive>,
    checkpoint: u32,
    state: SharedHistoryState,
}

#[async_trait]
impl Work for DownloadScpHistoryWork {
    fn name(&self) -> &str {
        "download-scp-history"
    }

    async fn run(&mut self, _ctx: &WorkContext) -> WorkOutcome {
        set_progress(
            &self.state,
            HistoryWorkStage::DownloadScp,
            "downloading SCP history",
        )
        .await;
        match self.archive.get_scp_history(self.checkpoint).await {
            Ok(entries) => {
                let mut guard = self.state.lock().await;
                guard.scp_history = entries;
                WorkOutcome::Success
            }
            Err(err) => WorkOutcome::Failed(format!("failed to download SCP history: {err}")),
        }
    }
}

/// Maximum number of concurrent download requests, matching stellar-core's
/// `MAX_CONCURRENT_SUBPROCESSES` limit.
const MAX_CONCURRENT_DOWNLOADS: usize = 16;

/// Returns non-empty, non-zero bucket hashes from a History Archive State.
///
/// Filters out the zero hash and the hash of the empty bucket, which are
/// sentinel values that should not be downloaded.
fn content_bucket_hashes(has: &HistoryArchiveState) -> Vec<Hash256> {
    let empty_bucket_hash = Hash256::hash(&[]);
    has.unique_bucket_hashes()
        .into_iter()
        .filter(|h| !h.is_zero() && *h != empty_bucket_hash)
        .collect()
}

fn find_header<'a>(
    headers: &'a [LedgerHeaderHistoryEntry],
    ledger_seq: u32,
    missing_label: &str,
) -> Result<&'a LedgerHeaderHistoryEntry, String> {
    headers
        .iter()
        .find(|header| header.header.ledger_seq == ledger_seq)
        .ok_or_else(|| format!("no header found for {missing_label} at ledger {ledger_seq}"))
}

/// IDs for registered download work items.
///
/// Returned by [`HistoryWorkBuilder::register`] to identify the work items
/// in the scheduler. These IDs can be used to:
/// - Query work status
/// - Add dependent work items
#[derive(Debug, Clone, Copy)]
pub struct HistoryWorkIds {
    /// ID of the HAS download work item.
    pub has: WorkId,
    /// ID of the bucket download work item.
    pub buckets: WorkId,
    /// ID of the ledger headers download work item.
    pub headers: WorkId,
    /// ID of the transactions download work item.
    pub transactions: WorkId,
    /// ID of the transaction results download work item.
    pub tx_results: WorkId,
    /// ID of the SCP history download work item.
    pub scp_history: WorkId,
}

/// Builder for registering history work items with a scheduler.
///
/// This is the primary interface for setting up history download workflows.
/// It creates work items with the correct dependency relationships and
/// registers them with a [`WorkScheduler`].
///
/// # Example
///
/// ```rust,ignore
/// use henyey_historywork::{HistoryWorkBuilder, SharedHistoryState};
/// use henyey_work::WorkScheduler;
/// use std::sync::Arc;
///
/// // Create shared state and builder
/// let state: SharedHistoryState = Default::default();
/// let builder = HistoryWorkBuilder::new(archive.clone(), checkpoint, state.clone());
///
/// // Register download work items
/// let mut scheduler = WorkScheduler::new();
/// let download_ids = builder.register(&mut scheduler);
///
/// // Run all work to completion
/// scheduler.run_until_done().await;
///
/// // Build checkpoint data from completed downloads
/// let data = build_checkpoint_data(&state).await?;
/// ```
pub struct HistoryWorkBuilder {
    archive: Arc<HistoryArchive>,
    checkpoint: u32,
    state: SharedHistoryState,
    bucket_dir: PathBuf,
}

impl HistoryWorkBuilder {
    /// Creates a new history work builder.
    ///
    /// # Arguments
    ///
    /// * `archive` - The history archive to download from
    /// * `checkpoint` - The checkpoint ledger sequence number
    /// * `state` - Shared state that will be populated by download work
    /// * `bucket_dir` - Directory where bucket files will be saved
    pub fn new(
        archive: Arc<HistoryArchive>,
        checkpoint: u32,
        state: SharedHistoryState,
        bucket_dir: PathBuf,
    ) -> Self {
        Self {
            archive,
            checkpoint,
            state,
            bucket_dir,
        }
    }

    /// Registers download work items with the scheduler.
    ///
    /// Creates and registers all download work items (HAS, buckets, headers,
    /// transactions, results, SCP) with proper dependency ordering. Each work
    /// item is configured with appropriate retry counts per CATCHUP_SPEC §9.1:
    /// HAS downloads use `RETRY_A_FEW` (10), bulk downloads use `RETRY_A_LOT` (32).
    ///
    /// # Returns
    ///
    /// [`HistoryWorkIds`] containing the scheduler IDs for all registered work.
    pub fn register(&self, scheduler: &mut WorkScheduler) -> HistoryWorkIds {
        let has_id = scheduler.add_work(
            Box::new(GetHistoryArchiveStateWork {
                archive: Arc::clone(&self.archive),
                checkpoint: self.checkpoint,
                state: Arc::clone(&self.state),
            }),
            vec![],
            RETRY_A_FEW,
        );

        // Spec: CATCHUP_SPEC §9.1 — bucket downloads use RETRY_A_LOT (32).
        let buckets_id = scheduler.add_work(
            Box::new(DownloadBucketsWork {
                archive: Arc::clone(&self.archive),
                state: Arc::clone(&self.state),
                bucket_dir: self.bucket_dir.clone(),
            }),
            vec![has_id],
            RETRY_A_LOT,
        );

        // Spec: CATCHUP_SPEC §9.1 — ledger header downloads use RETRY_A_LOT (32).
        let headers_id = scheduler.add_work(
            Box::new(DownloadLedgerHeadersWork {
                archive: Arc::clone(&self.archive),
                checkpoint: self.checkpoint,
                state: Arc::clone(&self.state),
            }),
            vec![has_id],
            RETRY_A_LOT,
        );

        // Spec: CATCHUP_SPEC §9.1 — transaction file downloads use RETRY_A_LOT (32).
        let tx_id = scheduler.add_work(
            Box::new(DownloadTransactionsWork {
                archive: Arc::clone(&self.archive),
                checkpoint: self.checkpoint,
                state: Arc::clone(&self.state),
            }),
            vec![headers_id],
            RETRY_A_LOT,
        );

        let tx_results_id = scheduler.add_work(
            Box::new(DownloadTxResultsWork {
                archive: Arc::clone(&self.archive),
                checkpoint: self.checkpoint,
                state: Arc::clone(&self.state),
            }),
            vec![headers_id, tx_id],
            RETRY_A_LOT,
        );

        let scp_id = scheduler.add_work(
            Box::new(DownloadScpHistoryWork {
                archive: Arc::clone(&self.archive),
                checkpoint: self.checkpoint,
                state: Arc::clone(&self.state),
            }),
            vec![headers_id],
            RETRY_A_FEW,
        );

        HistoryWorkIds {
            has: has_id,
            buckets: buckets_id,
            headers: headers_id,
            transactions: tx_id,
            tx_results: tx_results_id,
            scp_history: scp_id,
        }
    }
}

// ============================================================================
// Helper functions for accessing shared state
// ============================================================================

/// Retrieves the current progress indicator from shared state.
///
/// This function never fails and returns default progress if no work
/// has started yet.
pub async fn get_progress(state: &SharedHistoryState) -> HistoryWorkProgress {
    let guard = state.lock().await;
    guard.progress.clone()
}

// ============================================================================
// Checkpoint Data Assembly
// ============================================================================

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
/// Returns an error if the HAS is not available (other fields may be empty).
pub async fn build_checkpoint_data(state: &SharedHistoryState) -> Result<CheckpointData> {
    let guard = state.lock().await;
    let has = guard
        .has
        .clone()
        .ok_or_else(|| anyhow!("missing History Archive State"))?;

    Ok(CheckpointData {
        has,
        bucket_dir: guard
            .bucket_dir
            .clone()
            .ok_or_else(|| anyhow!("bucket directory not set"))?,
        headers: guard.headers.clone(),
        transactions: guard.transactions.clone(),
        tx_results: guard.tx_results.clone(),
        scp_history: guard.scp_history.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
