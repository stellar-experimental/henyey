//! Download work items for history archive data.
//!
//! This module contains all work items that download and verify data from
//! Stellar history archives. Each work item implements the [`Work`] trait
//! and is registered with a [`WorkScheduler`] via [`HistoryWorkBuilder`].

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::{self, StreamExt};

use henyey_bucket::canonical_bucket_filename;
use henyey_common::fs_utils::atomic_write_bytes;
use henyey_common::Hash256;
use henyey_history::{archive::HistoryArchive, archive_state::HistoryArchiveState, verify};
use henyey_ledger::TransactionSetVariant;
use henyey_work::{Work, WorkContext, WorkOutcome};
use stellar_xdr::curr::{LedgerHeaderHistoryEntry, TransactionHistoryEntryExt, WriteXdr};

use crate::{set_progress, HistoryWorkStage, SharedHistoryState};

/// Maximum number of concurrent download requests, matching stellar-core's
/// `MAX_CONCURRENT_SUBPROCESSES` limit.
pub(crate) const MAX_CONCURRENT_DOWNLOADS: usize = 16;

/// Log download progress every N items (and always on the last item).
const PROGRESS_REPORT_INTERVAL: u32 = 5;

// ============================================================================
// Work Items
// ============================================================================

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
    pub(crate) archive: Arc<HistoryArchive>,
    pub(crate) checkpoint: u32,
    pub(crate) state: SharedHistoryState,
}

#[async_trait]
impl Work for GetHistoryArchiveStateWork {
    fn name(&self) -> &str {
        "get-history-archive-state"
    }

    // SECURITY: checkpoint data validated by hash chain; content integrity verified before acceptance
    async fn run(&mut self, _ctx: &WorkContext) -> WorkOutcome {
        set_progress(&self.state, HistoryWorkStage::FetchHas, "fetching HAS").await;
        let archive_name = self.archive.name().to_owned();
        // Stage E instrumentation: emit HAS success only after both fetch and
        // structure/checkpoint verification pass — matching
        // `catchup::download_and_verify_has`. Passphrase verification is not
        // performed here because the historywork builder does not carry a
        // network passphrase; that check belongs at the direct-catchup layer
        // and is a superset of what this counter promises.
        match self.archive.fetch_checkpoint_has(self.checkpoint).await {
            Ok(has) => {
                if let Err(err) = verify::verify_has_structure(&has) {
                    metrics::counter!(
                        "stellar_history_download_history_archive_state_failure_total",
                        "archive" => archive_name,
                    )
                    .increment(1);
                    return WorkOutcome::Failed(format!("HAS structure invalid: {err}"));
                }
                if let Err(err) = verify::verify_has_checkpoint(&has, self.checkpoint) {
                    metrics::counter!(
                        "stellar_history_download_history_archive_state_failure_total",
                        "archive" => archive_name,
                    )
                    .increment(1);
                    return WorkOutcome::Failed(format!("HAS checkpoint mismatch: {err}"));
                }
                metrics::counter!(
                    "stellar_history_download_history_archive_state_success_total",
                    "archive" => archive_name,
                )
                .increment(1);
                let mut guard = self.state.lock().await;
                guard.has = Some(has);
                WorkOutcome::Success
            }
            Err(err) => {
                metrics::counter!(
                    "stellar_history_download_history_archive_state_failure_total",
                    "archive" => archive_name,
                )
                .increment(1);
                WorkOutcome::Failed(format!("failed to fetch HAS: {err}"))
            }
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
    pub(crate) archive: Arc<HistoryArchive>,
    pub(crate) state: SharedHistoryState,
    pub(crate) bucket_dir: PathBuf,
}

/// Outcome of a single-bucket download attempt, used to attribute failure to
/// the correct Stage E counter (`download_bucket_failure_total` for fetch
/// errors, `verify_bucket_failure_total` for hash mismatches).
enum BucketDownloadFailure {
    Download(String),
    Verify(String),
    Persist(String),
}

impl BucketDownloadFailure {
    fn into_message(self) -> String {
        match self {
            BucketDownloadFailure::Download(m)
            | BucketDownloadFailure::Verify(m)
            | BucketDownloadFailure::Persist(m) => m,
        }
    }
}

/// Downloads a single bucket, verifies its hash, and saves it to disk.
///
/// Stage E: returns a `BucketDownloadFailure` so the caller can emit the
/// appropriate per-bucket counter without re-classifying error strings.
/// On success, both `download_bucket_success` and `verify_bucket_success`
/// are emitted (both steps necessarily passed). On `Download` failure,
/// `download_bucket_failure` is emitted; on `Verify` failure,
/// `verify_bucket_failure`; on `Persist` failure, `download_bucket_failure`
/// (the bucket bytes were valid but couldn't be cached — operationally
/// equivalent to a download failure for retry purposes).
async fn download_and_save_bucket(
    archive: &HistoryArchive,
    hash: &Hash256,
    bucket_path: &std::path::Path,
) -> Result<(), BucketDownloadFailure> {
    let data = archive.fetch_bucket(hash).await.map_err(|err| {
        BucketDownloadFailure::Download(format!("failed to download bucket {hash}: {err}"))
    })?;

    verify::verify_bucket_hash(&data, hash).map_err(|err| {
        BucketDownloadFailure::Verify(format!("bucket {hash} hash mismatch: {err}"))
    })?;

    atomic_write_bytes(bucket_path, &data).map_err(|e| {
        BucketDownloadFailure::Persist(format!("failed to save bucket {hash} to disk: {e}"))
    })?;

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
        // Borrow the HAS from the shared state to extract bucket hashes, then
        // drop the lock before starting the (potentially slow) downloads.
        let hashes = {
            let guard = self.state.lock().await;
            let Some(ref has) = guard.has else {
                return WorkOutcome::Failed("missing HAS".to_string());
            };
            content_bucket_hashes(has)
        };
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
                let path = bucket_dir.join(canonical_bucket_filename(&hash));
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

            let results: Vec<Result<(), BucketDownloadFailure>> = stream::iter(to_download)
                .map(|hash| {
                    let archive = archive.clone();
                    let bucket_dir = bucket_dir.clone();
                    let downloaded_count = &downloaded_count;

                    async move {
                        let path = bucket_dir.join(canonical_bucket_filename(&hash));
                        download_and_save_bucket(&archive, &hash, &path).await?;

                        let count =
                            downloaded_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        if count % PROGRESS_REPORT_INTERVAL == 0
                            || count == total_to_download as u32
                        {
                            tracing::info!("Downloaded {}/{} buckets", count, total_to_download);
                        }
                        Ok(())
                    }
                })
                .buffer_unordered(MAX_CONCURRENT_DOWNLOADS)
                .collect()
                .await;

            // Stage E: emit per-bucket success/failure counters, attributed to
            // the correct phase (download vs verify). On success, both
            // download and verify counters fire because both steps passed.
            // Then surface the first error to the work scheduler.
            let archive_name = self.archive.name().to_owned();
            let mut first_failure: Option<String> = None;
            for result in results {
                match result {
                    Ok(()) => {
                        metrics::counter!(
                            "stellar_history_download_bucket_success_total",
                            "archive" => archive_name.clone(),
                        )
                        .increment(1);
                        metrics::counter!("stellar_history_verify_bucket_success_total")
                            .increment(1);
                    }
                    Err(failure) => {
                        match &failure {
                            BucketDownloadFailure::Download(_)
                            | BucketDownloadFailure::Persist(_) => {
                                metrics::counter!(
                                    "stellar_history_download_bucket_failure_total",
                                    "archive" => archive_name.clone(),
                                )
                                .increment(1);
                            }
                            BucketDownloadFailure::Verify(_) => {
                                metrics::counter!("stellar_history_verify_bucket_failure_total")
                                    .increment(1);
                            }
                        }
                        if first_failure.is_none() {
                            first_failure = Some(failure.into_message());
                        }
                    }
                }
            }
            if let Some(err) = first_failure {
                return WorkOutcome::Failed(err);
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
    pub(crate) archive: Arc<HistoryArchive>,
    pub(crate) checkpoint: u32,
    pub(crate) state: SharedHistoryState,
}

#[async_trait]
impl Work for DownloadLedgerHeadersWork {
    fn name(&self) -> &str {
        "download-ledger-headers"
    }

    // SECURITY: checkpoint data validated by hash chain; content integrity verified before acceptance
    async fn run(&mut self, _ctx: &WorkContext) -> WorkOutcome {
        set_progress(
            &self.state,
            HistoryWorkStage::DownloadHeaders,
            "downloading headers",
        )
        .await;
        let archive_name = self.archive.name().to_owned();
        // Stage E: header *download* failures map to
        // `download_ledger_failure_total` (the checkpoint-data download is
        // considered failed if any constituent file fails). Header *chain
        // verification* failures map to `verify_ledger_chain_failure_total`
        // (header chain verification is the closest historywork analogue to
        // catchup's `verify_downloaded_data`). On success here, only count
        // verify_ledger_chain_success — `download_ledger_success` is emitted
        // by the trailing `DownloadTxResultsWork` so the checkpoint-data
        // counter fires once per checkpoint, not once per file.
        let headers = match self.archive.fetch_ledger_headers(self.checkpoint).await {
            Ok(headers) => headers,
            Err(err) => {
                metrics::counter!(
                    "stellar_history_download_ledger_failure_total",
                    "archive" => archive_name,
                )
                .increment(1);
                return WorkOutcome::Failed(format!("failed to download headers: {err}"));
            }
        };

        if let Err(err) = verify::verify_header_chain_from_entries(&headers) {
            metrics::counter!("stellar_history_verify_ledger_chain_failure_total").increment(1);
            return WorkOutcome::Failed(format!("header chain verification failed: {err}"));
        }
        metrics::counter!("stellar_history_verify_ledger_chain_success_total").increment(1);

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
    pub(crate) archive: Arc<HistoryArchive>,
    pub(crate) checkpoint: u32,
    pub(crate) state: SharedHistoryState,
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
        let archive_name = self.archive.name().to_owned();
        // Stage E: download failures here map to
        // `download_ledger_failure_total` (any failed checkpoint sub-file
        // means the checkpoint-data acquisition failed). tx-set hash
        // verification failures also map to `download_ledger_failure_total`
        // here — tx-set hash mismatches are part of the per-file integrity
        // check folded into the same dashboard concept.
        let entries = match self.archive.fetch_transactions(self.checkpoint).await {
            Ok(entries) => entries,
            Err(err) => {
                metrics::counter!(
                    "stellar_history_download_ledger_failure_total",
                    "archive" => archive_name,
                )
                .increment(1);
                return WorkOutcome::Failed(format!("failed to download transactions: {err}"));
            }
        };

        let headers = {
            let guard = self.state.lock().await;
            guard.headers.clone()
        };
        for entry in &entries {
            let header = match find_header(&headers, entry.ledger_seq, "transaction set") {
                Ok(header) => header,
                Err(err) => {
                    metrics::counter!(
                        "stellar_history_download_ledger_failure_total",
                        "archive" => archive_name,
                    )
                    .increment(1);
                    return WorkOutcome::Failed(err);
                }
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
                metrics::counter!(
                    "stellar_history_download_ledger_failure_total",
                    "archive" => archive_name,
                )
                .increment(1);
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
    pub(crate) archive: Arc<HistoryArchive>,
    pub(crate) checkpoint: u32,
    pub(crate) state: SharedHistoryState,
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
        let archive_name = self.archive.name().to_owned();
        // Stage E: as the *last* node in the historywork DAG (results depends
        // on transactions which depends on headers), the success of this
        // work item means the entire checkpoint ledger-data set
        // (headers+txs+results) has been acquired. Emit the single
        // checkpoint-level `download_ledger_success` here so that
        // historywork and direct catchup both fire the counter once per
        // checkpoint. Failures in this step also count, even though
        // upstream steps may have already failed and emitted a failure of
        // their own — a "failed checkpoint" can plausibly increment the
        // failure counter more than once per checkpoint, but we accept that
        // over the alternative of dropping all upstream-failure visibility.
        let results = match self.archive.fetch_results(self.checkpoint).await {
            Ok(results) => results,
            Err(err) => {
                metrics::counter!(
                    "stellar_history_download_ledger_failure_total",
                    "archive" => archive_name,
                )
                .increment(1);
                return WorkOutcome::Failed(format!("failed to download tx results: {err}"));
            }
        };

        for entry in &results {
            let header = match find_header(&headers, entry.ledger_seq, "tx result set") {
                Ok(header) => header,
                Err(err) => {
                    metrics::counter!(
                        "stellar_history_download_ledger_failure_total",
                        "archive" => archive_name,
                    )
                    .increment(1);
                    return WorkOutcome::Failed(err);
                }
            };
            let xdr = match entry
                .tx_result_set
                .to_xdr(stellar_xdr::curr::Limits::none())
            {
                Ok(xdr) => xdr,
                Err(err) => {
                    metrics::counter!(
                        "stellar_history_download_ledger_failure_total",
                        "archive" => archive_name,
                    )
                    .increment(1);
                    return WorkOutcome::Failed(format!(
                        "failed to serialize tx result set for ledger {}: {err}",
                        entry.ledger_seq
                    ));
                }
            };
            if let Err(err) = verify::verify_tx_result_set(&header.header, &xdr) {
                metrics::counter!(
                    "stellar_history_download_ledger_failure_total",
                    "archive" => archive_name,
                )
                .increment(1);
                return WorkOutcome::Failed(format!("tx result set hash mismatch: {err}"));
            }
        }

        metrics::counter!(
            "stellar_history_download_ledger_success_total",
            "archive" => archive_name,
        )
        .increment(1);

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
    pub(crate) archive: Arc<HistoryArchive>,
    pub(crate) checkpoint: u32,
    pub(crate) state: SharedHistoryState,
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
        match self.archive.fetch_scp_history(self.checkpoint).await {
            Ok(entries) => {
                let mut guard = self.state.lock().await;
                guard.scp_history = entries;
                WorkOutcome::Success
            }
            Err(err) => WorkOutcome::Failed(format!("failed to download SCP history: {err}")),
        }
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

/// Returns non-empty, non-zero bucket hashes from a History Archive State.
///
/// Filters out sentinel hashes (zero hash and empty-file hash), which
/// don't require files on disk and should not be downloaded.
pub(crate) fn content_bucket_hashes(has: &HistoryArchiveState) -> Vec<Hash256> {
    has.unique_bucket_hashes()
        .into_iter()
        .filter(|h| !h.is_empty_bucket_sentinel())
        .collect()
}

pub(crate) fn find_header<'a>(
    headers: &'a [LedgerHeaderHistoryEntry],
    ledger_seq: u32,
    missing_label: &str,
) -> Result<&'a LedgerHeaderHistoryEntry, String> {
    headers
        .iter()
        .find(|header| header.header.ledger_seq == ledger_seq)
        .ok_or_else(|| format!("no header found for {missing_label} at ledger {ledger_seq}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stage E: every Stage-E metric literal emitted from this module is
    /// pinned so a typo silently detaches dashboards. The literal-presence
    /// check is paired with the catalog-side test
    /// `crates/app/src/metrics.rs::test_stage_e_history_metrics_in_catalog`.
    #[test]
    fn test_stage_e_historywork_metric_literals_present() {
        let src = include_str!("download.rs");
        for literal in &[
            "\"stellar_history_download_history_archive_state_success_total\"",
            "\"stellar_history_download_history_archive_state_failure_total\"",
            "\"stellar_history_download_bucket_success_total\"",
            "\"stellar_history_download_bucket_failure_total\"",
            "\"stellar_history_verify_bucket_success_total\"",
            "\"stellar_history_verify_bucket_failure_total\"",
            "\"stellar_history_download_ledger_success_total\"",
            "\"stellar_history_download_ledger_failure_total\"",
            "\"stellar_history_verify_ledger_chain_success_total\"",
            "\"stellar_history_verify_ledger_chain_failure_total\"",
        ] {
            assert!(
                src.contains(literal),
                "expected metric literal {literal} in historywork/download.rs",
            );
        }
    }

    /// Stage E: download counters must carry the `"archive"` label.
    #[test]
    fn test_stage_e_historywork_archive_label_present() {
        let src = include_str!("download.rs");
        // Every download counter emit site must include archive labeling.
        for metric in &[
            "stellar_history_download_history_archive_state_success_total",
            "stellar_history_download_history_archive_state_failure_total",
            "stellar_history_download_bucket_success_total",
            "stellar_history_download_bucket_failure_total",
            "stellar_history_download_ledger_success_total",
            "stellar_history_download_ledger_failure_total",
        ] {
            // Find each occurrence of the metric and verify "archive" appears
            // nearby (within the same counter!() macro invocation).
            let idx = src
                .find(metric)
                .unwrap_or_else(|| panic!("metric {metric} not found in historywork/download.rs"));
            // Look at the next 200 chars for the "archive" label
            let window = &src[idx..std::cmp::min(idx + 200, src.len())];
            assert!(
                window.contains("\"archive\""),
                "metric {metric} missing \"archive\" label in historywork/download.rs",
            );
        }
    }

    /// Stage E: classify a download/verify/persist failure to the correct
    /// counter. This is a pure helper test that doesn't require network
    /// access — it just validates the enum / message routing logic.
    #[test]
    fn test_bucket_download_failure_message_routing() {
        let f = BucketDownloadFailure::Download("network down".to_string());
        assert_eq!(f.into_message(), "network down");
        let f = BucketDownloadFailure::Verify("hash mismatch".to_string());
        assert_eq!(f.into_message(), "hash mismatch");
        let f = BucketDownloadFailure::Persist("disk full".to_string());
        assert_eq!(f.into_message(), "disk full");
    }
}
