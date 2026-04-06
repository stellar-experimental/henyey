//! Ledger replay logic for catchup: re-executing transactions via close_ledger.

use crate::{verify, HistoryError, Result};
use henyey_common::Hash256;
use std::sync::Arc;

use henyey_ledger::{LedgerCloseData, LedgerManager, TransactionSetVariant};
use stellar_xdr::curr::{LedgerHeader, LedgerUpgrade, Limits, ReadXdr, WriteXdr};
use tracing::{debug, info, warn};

use super::{CatchupManager, CatchupStatus, LedgerData};

/// Maximum number of retry attempts for the download-and-replay pipeline.
///
/// Matches stellar-core's `BasicWork::RETRY_A_FEW` used by
/// `DownloadApplyTxsWork` (a `BatchWork` subclass). On each retry the
/// replay start is recalculated from the current LCL, so partial
/// progress is preserved (mirroring stellar-core's `resetIter()`).
pub(super) const REPLAY_RETRY_COUNT: u32 = 5;

/// Decode ledger upgrades from a header's SCP value.
///
/// Each `upgrade` in `header.scp_value.upgrades` is an XDR-encoded `LedgerUpgrade`.
/// Invalid entries are skipped with a warning.
pub(super) fn decode_upgrades_from_header(header: &LedgerHeader) -> Vec<LedgerUpgrade> {
    header
        .scp_value
        .upgrades
        .iter()
        .filter_map(|upgrade| {
            let bytes = upgrade.0.as_slice();
            match LedgerUpgrade::from_xdr(bytes, Limits::none()) {
                Ok(decoded) => Some(decoded),
                Err(err) => {
                    warn!(error = %err, "Failed to decode ledger upgrade during replay");
                    None
                }
            }
        })
        .collect()
}

impl CatchupManager {
    /// Verify the downloaded ledger data.
    pub(super) fn verify_downloaded_data(
        &self,
        ledger_data: &[LedgerData],
        anchors: &verify::ChainTrustAnchors,
    ) -> Result<()> {
        if ledger_data.is_empty() {
            return Ok(());
        }

        // Extract headers for chain verification
        let headers: Vec<_> = ledger_data.iter().map(|d| d.header.clone()).collect();

        // Skip header chain and trust anchor verification when verify_results
        // is false (maps from CatchupOptions::verify_headers). This allows
        // synthetic tests to bypass chain integrity checks.
        if self.replay_config.verify_results {
            verify::verify_header_chain(&headers)?;

            // Verify trust anchors (spec §9.2–§9.5): ensures the chain connects
            // to externally trusted state (checkpoint header hash, LCL hash).
            verify::verify_chain_anchors(&headers, anchors)?;
        }

        // Verify transaction sets match header hashes
        for data in ledger_data {
            if let Some(entry) = data.tx_history_entry.as_ref() {
                let tx_set = match &entry.ext {
                    stellar_xdr::curr::TransactionHistoryEntryExt::V0 => {
                        TransactionSetVariant::Classic(entry.tx_set.clone())
                    }
                    stellar_xdr::curr::TransactionHistoryEntryExt::V1(set) => {
                        TransactionSetVariant::Generalized(set.clone())
                    }
                };
                verify::verify_tx_set(&data.header, &tx_set)?;
            }
            if let Some(entry) = data.tx_result_entry.as_ref() {
                let xdr = entry
                    .tx_result_set
                    .to_xdr(stellar_xdr::curr::Limits::none())
                    .map_err(|e| {
                        HistoryError::CatchupFailed(format!(
                            "Failed to serialize tx result set for ledger {}: {}",
                            data.header.ledger_seq, e
                        ))
                    })?;
                verify::verify_tx_result_set(&data.header, &xdr)?;
            }
        }

        info!("Verified header chain for {} ledgers", headers.len());
        Ok(())
    }

    /// Download, verify, and replay ledgers from `replay_start` to `target`
    /// with bounded retry on transient failures.
    ///
    /// Matches stellar-core's `DownloadApplyTxsWork` which uses
    /// `BatchWork(RETRY_A_FEW)`. On each retry, the replay start is
    /// recalculated from the ledger manager's current LCL (mirroring
    /// `DownloadApplyTxsWork::resetIter()` which resets
    /// `mCheckpointToQueue` to `checkpointContainingLedger(LCL + 1)`).
    ///
    /// Fatal errors (verification/integrity failures) are NOT retried —
    /// only transient errors (network, download, etc.) trigger a retry.
    pub(super) async fn download_verify_and_replay_with_retry(
        &mut self,
        target: u32,
        ledger_manager: &LedgerManager,
    ) -> Result<(LedgerHeader, Hash256, u32)> {
        let mut last_error: Option<HistoryError> = None;

        for attempt in 0..=REPLAY_RETRY_COUNT {
            if attempt > 0 {
                let delay_ms = 200 * (1 << (attempt - 1).min(4));
                warn!(
                    attempt,
                    max_attempts = REPLAY_RETRY_COUNT + 1,
                    delay_ms,
                    error = %last_error.as_ref().unwrap(),
                    "Retrying download-and-replay pipeline"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }

            // Recalculate replay start from current LCL (matching resetIter()).
            let current_lcl = ledger_manager.current_header().ledger_seq;
            let replay_first = current_lcl + 1;

            if replay_first > target {
                // Already past target — previous partial replay succeeded fully.
                let final_header = ledger_manager.current_header();
                let final_hash = ledger_manager.current_header_hash();
                let ledgers_applied = target.saturating_sub(current_lcl);
                return Ok((final_header, final_hash, ledgers_applied));
            }

            // Download from the checkpoint containing replay_first - 1 (the LCL).
            let download_from = current_lcl;

            match self
                .download_verify_and_replay_once(download_from, target, ledger_manager)
                .await
            {
                Ok(result) => return Ok(result),
                Err(e) => {
                    if e.is_fatal_catchup_failure() {
                        warn!(
                            attempt,
                            error = %e,
                            "Fatal error during replay — not retrying"
                        );
                        return Err(e);
                    }
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            HistoryError::CatchupFailed(
                "download-and-replay exhausted all retry attempts".to_string(),
            )
        }))
    }

    /// Single attempt at download + verify + replay from `download_from` to `target`.
    async fn download_verify_and_replay_once(
        &mut self,
        download_from: u32,
        target: u32,
        ledger_manager: &LedgerManager,
    ) -> Result<(LedgerHeader, Hash256, u32)> {
        use henyey_common::NetworkId;

        // Download ledger data for replay
        self.update_progress(
            CatchupStatus::DownloadingLedgers,
            4,
            "Downloading ledger data",
        );
        let ledger_data = self.download_ledger_data(download_from, target).await?;

        // Verify the header chain
        self.update_progress(CatchupStatus::Verifying, 5, "Verifying header chain");
        let anchor_hash = ledger_manager.current_header_hash();
        let anchors = verify::ChainTrustAnchors {
            previous_ledger_hash: Some(anchor_hash),
            ..Default::default()
        };
        self.verify_downloaded_data(&ledger_data, &anchors)?;

        // Replay ledgers via close_ledger
        self.update_progress(CatchupStatus::Replaying, 6, "Replaying ledgers");
        self.replay_via_close_ledger(ledger_manager, &ledger_data)
            .await?;

        let network_id = NetworkId(ledger_manager.network_id().0);
        self.persist_ledger_history(&ledger_data, &network_id)?;

        let final_header = ledger_manager.current_header();
        let final_hash = ledger_manager.current_header_hash();
        let ledgers_applied = final_header.ledger_seq.saturating_sub(download_from);

        Ok((final_header, final_hash, ledgers_applied))
    }

    /// Replay ledgers by calling `LedgerManager::close_ledger()` for each one.
    ///
    /// This eliminates the duplicate replay implementation and uses the same
    /// code path as live ledger close, ensuring consistent behavior for:
    /// - Offer store maintenance (populated by `initialize()`, updated by `close_ledger()`)
    /// - Soroban state size tracking
    /// - Eviction scanning
    /// - Bucket list updates
    pub(super) async fn replay_via_close_ledger(
        &mut self,
        ledger_manager: &LedgerManager,
        ledger_data: &[LedgerData],
    ) -> Result<()> {
        if ledger_data.is_empty() {
            return Err(HistoryError::CatchupFailed(
                "no ledger data to replay".to_string(),
            ));
        }

        let total = ledger_data.len();
        // CATCHUP_SPEC §5.6: Publish queue backpressure state.
        // When the queue exceeds PUBLISH_QUEUE_MAX_SIZE, replay pauses until
        // it drains to PUBLISH_QUEUE_UNBLOCK_APPLICATION.
        let mut pq_fell_behind = false;

        for (i, data) in ledger_data.iter().enumerate() {
            self.progress.current_ledger = data.header.ledger_seq;

            // Apply publish queue backpressure if enabled (offline catchup).
            if self.replay_config.wait_for_publish {
                self.wait_for_publish_queue(&mut pq_fell_behind).await?;
            }

            // Decode upgrades from the header's scp_value.upgrades
            let upgrades = decode_upgrades_from_header(&data.header);

            let close_data = LedgerCloseData::new(
                data.header.ledger_seq,
                data.tx_set.clone(),
                data.header.scp_value.close_time.0,
                ledger_manager.current_header_hash(),
            )
            .with_stellar_value_ext(data.header.scp_value.ext.clone())
            .with_upgrades(upgrades);

            let result = ledger_manager.close_ledger(close_data, None).map_err(|e| {
                HistoryError::CatchupFailed(format!(
                    "close_ledger failed at ledger {}: {}",
                    data.header.ledger_seq, e
                ))
            })?;

            // Verify computed header hash matches archive
            if self.replay_config.verify_bucket_list {
                let expected_hash =
                    henyey_ledger::compute_header_hash(&data.header).map_err(|e| {
                        HistoryError::CatchupFailed(format!(
                            "Failed to compute header hash for ledger {}: {}",
                            data.header.ledger_seq, e
                        ))
                    })?;
                if result.header_hash != expected_hash {
                    return Err(HistoryError::CatchupFailed(format!(
                        "Header hash mismatch at ledger {}: computed={}, expected={}",
                        data.header.ledger_seq,
                        result.header_hash.to_hex(),
                        expected_hash.to_hex()
                    )));
                }
            }

            // Emit metadata to SQLite and external consumers (e.g., stellar-rpc's
            // meta pipe in bounded replay mode: `catchup --metadata-output-stream fd:3`).
            if let Some(meta) = result.meta {
                self.emit_meta(data.header.ledger_seq, meta);
            }

            debug!(
                "Replayed ledger {}/{} via close_ledger: seq={}",
                i + 1,
                total,
                data.header.ledger_seq
            );
        }

        Ok(())
    }

    /// Wait until the publish queue is below the backpressure threshold.
    ///
    /// CATCHUP_SPEC §5.6 / §11.4: Uses hysteresis (high/low water marks) to
    /// avoid oscillation. Sets `pq_fell_behind` when queue > 16, clears it
    /// when queue <= 8.
    async fn wait_for_publish_queue(&self, pq_fell_behind: &mut bool) -> Result<()> {
        use crate::publish_queue::{
            PublishQueue, PUBLISH_QUEUE_MAX_SIZE, PUBLISH_QUEUE_UNBLOCK_APPLICATION,
        };

        let pq = PublishQueue::new(Arc::clone(&self.db));
        loop {
            let queue_len = pq.len()?;

            if queue_len <= PUBLISH_QUEUE_UNBLOCK_APPLICATION {
                *pq_fell_behind = false;
            }
            if queue_len > PUBLISH_QUEUE_MAX_SIZE {
                *pq_fell_behind = true;
            }

            if !*pq_fell_behind {
                return Ok(());
            }

            debug!(
                queue_len,
                "Publish queue backpressure: waiting for queue to drain"
            );
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HistoryError;

    /// Verify REPLAY_RETRY_COUNT matches stellar-core's RETRY_A_FEW = 5.
    #[test]
    fn test_replay_retry_count_matches_stellar_core() {
        assert_eq!(REPLAY_RETRY_COUNT, 5, "must match BasicWork::RETRY_A_FEW");
    }

    /// Fatal errors must not be retried — verify the classification.
    #[test]
    fn test_fatal_errors_not_retriable() {
        let fatal_errors: Vec<HistoryError> = vec![
            HistoryError::VerificationFailed("hash mismatch".to_string()),
            HistoryError::InvalidPreviousHash { ledger: 100 },
            HistoryError::InvalidTxSetHash { ledger: 100 },
            HistoryError::InvalidSequence {
                expected: 100,
                got: 200,
            },
            HistoryError::CorruptHeader {
                ledger: 100,
                detail: "bad encoding".to_string(),
            },
        ];
        for err in &fatal_errors {
            assert!(err.is_fatal_catchup_failure(), "expected fatal: {}", err);
        }
    }

    /// Transient errors should be retriable (i.e., NOT fatal).
    #[test]
    fn test_transient_errors_are_retriable() {
        let transient_errors: Vec<HistoryError> = vec![
            HistoryError::ArchiveUnreachable("timeout".to_string()),
            HistoryError::DownloadFailed("connection reset".to_string()),
            HistoryError::CatchupFailed("close_ledger failed at ledger 100".to_string()),
            HistoryError::NotFound("missing file".to_string()),
            HistoryError::HttpStatus {
                url: "http://archive.example.com".to_string(),
                status: 503,
            },
        ];
        for err in &transient_errors {
            assert!(
                !err.is_fatal_catchup_failure(),
                "expected transient (retriable): {}",
                err
            );
        }
    }

    /// [AUDIT-YH2] verify_tx_set must return a fatal error for mismatched tx set hashes.
    /// Before fix: verify_downloaded_data logged a warning and continued.
    /// After fix: the error propagates, halting replay.
    #[test]
    fn test_audit_yh2_tx_set_mismatch_is_error() {
        use stellar_xdr::curr::{Hash, LedgerHeader, StellarValue, TransactionSet};

        // Create a header with a specific tx_set_hash
        let mut header = LedgerHeader::default();
        header.ledger_seq = 100;
        header.scp_value = StellarValue {
            tx_set_hash: Hash([0xAA; 32]), // Expected hash
            ..Default::default()
        };

        // Create an empty tx set — its hash will NOT match [0xAA; 32]
        let tx_set = henyey_ledger::TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash([0; 32]),
            txs: vec![].try_into().unwrap(),
        });

        let result = crate::verify::verify_tx_set(&header, &tx_set);
        assert!(result.is_err(), "Mismatched tx set hash must be an error");
        let err = result.unwrap_err();
        assert!(
            err.is_fatal_catchup_failure(),
            "Tx set hash mismatch must be classified as fatal: {}",
            err
        );
    }

    /// [AUDIT-YH2] verify_tx_result_set must return a fatal error for mismatched result hashes.
    #[test]
    fn test_audit_yh2_tx_result_set_mismatch_is_error() {
        use stellar_xdr::curr::{Hash, LedgerHeader};

        let mut header = LedgerHeader::default();
        header.ledger_seq = 100;
        header.tx_set_result_hash = Hash([0xBB; 32]); // Expected hash

        // Provide a result set whose hash won't match [0xBB; 32]
        let fake_result_xdr = b"not the real result set";

        let result = crate::verify::verify_tx_result_set(&header, fake_result_xdr);
        assert!(
            result.is_err(),
            "Mismatched tx result hash must be an error"
        );
        let err = result.unwrap_err();
        assert!(
            err.is_fatal_catchup_failure(),
            "Tx result hash mismatch must be classified as fatal: {}",
            err
        );
    }

    /// Verify the exponential backoff formula used in retry.
    #[test]
    fn test_retry_backoff_formula() {
        // delay_ms = 200 * 2^(attempt-1), capped at 2^4 = 16
        let delays: Vec<u64> = (1..=REPLAY_RETRY_COUNT)
            .map(|attempt| 200 * (1u64 << (attempt - 1).min(4)))
            .collect();
        assert_eq!(delays, vec![200, 400, 800, 1600, 3200]);
    }
}
