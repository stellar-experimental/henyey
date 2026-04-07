//! Builder for registering history work items with a scheduler.
//!
//! This module provides [`HistoryWorkBuilder`] and [`HistoryWorkIds`] for
//! setting up download work item DAGs with proper dependency ordering.

use std::path::PathBuf;
use std::sync::Arc;

use henyey_history::{
    archive::HistoryArchive,
    download::{RETRY_A_FEW, RETRY_A_LOT},
};
use henyey_work::{WorkId, WorkScheduler};

use crate::download::{
    DownloadBucketsWork, DownloadLedgerHeadersWork, DownloadScpHistoryWork,
    DownloadTransactionsWork, DownloadTxResultsWork, GetHistoryArchiveStateWork,
};
use crate::SharedHistoryState;

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
    /// HAS downloads use `RETRY_A_FEW` (5), bulk downloads use `RETRY_A_LOT` (32).
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
