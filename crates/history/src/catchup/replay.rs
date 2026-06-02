//! Ledger replay logic for catchup: re-executing transactions via close_ledger.

use crate::{verify, HistoryError, Result};
use std::sync::Arc;

use henyey_common::protocol::LclContext;
use henyey_common::Hash256;
use henyey_ledger::{HeaderSnapshot, LedgerCloseData, LedgerManager};
use stellar_xdr::curr::{
    LedgerHeader, LedgerHeaderHistoryEntry, LedgerUpgrade, Limits, ReadXdr, WriteXdr,
};
use tracing::{debug, info, warn};

use super::{CatchupManager, CatchupStatus, LedgerData};

/// Decision returned by [`knit_to_lcl_decision`] for a single archive
/// `LedgerHeaderHistoryEntry`, mirroring the five-case decision matrix in
/// stellar-core `ApplyCheckpointWork::getNextLedgerCloseData()`
/// (CATCHUP_SPEC §11.2):
///
/// | Case | Condition | Result |
/// |------|-----------|--------|
/// | 1 (skip-old) | `entry.seq + 1 < lcl.seq` | `Ok(Skip)` |
/// | 2 (LCL predecessor knit) | `entry.seq + 1 == lcl.seq`, hashes match | `Ok(Skip)` |
/// | 3 (LCL overlap knit) | `entry.seq == lcl.seq`, hashes match | `Ok(Skip)` |
/// | 4 (apply) | `entry.seq == lcl.seq + 1`, prev-hash matches | `Ok(Apply)` |
/// | 5 (overshoot) | `entry.seq > lcl.seq + 1` | `Err(KnitOvershot)` |
///
/// Hash mismatches in cases 2/3/4 return the appropriate fatal
/// [`HistoryError`] variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KnitDecision {
    /// Drop the entry: it's at or below LCL and (where required) its hash
    /// agrees with the local LCL chain. The entry must not be replayed.
    Skip,
    /// Apply the entry: it is exactly `lcl + 1` and its
    /// `previousLedgerHash` matches the local LCL hash.
    Apply,
}

/// Apply the §11.2 5-case knit-to-LCL decision matrix to a single archive
/// header entry.
///
/// Mirrors stellar-core `ApplyCheckpointWork::getNextLedgerCloseData()` at
/// the pinned `stellar-core/v26.0.1` submodule. The comparison order
/// (case 1 → 2 → 3 → 5 → 4) is preserved exactly, as are the per-case
/// field selections (`entry.hash` vs `entry.header.previous_ledger_hash`).
///
/// Returns:
/// - `Ok(Skip)` for cases 1/2/3 when hashes agree.
/// - `Ok(Apply)` for case 4 when the previous-hash check succeeds.
/// - `Err(KnitLclPredecessorHashMismatch)` for case 2 mismatch.
/// - `Err(KnitLclHashMismatch)` for case 3 mismatch.
/// - `Err(KnitCurrentLedgerPrevHashMismatch)` for case 4 prev-hash
///   mismatch. Distinct from case 3 to match stellar-core's two distinct
///   error messages.
/// - `Err(KnitOvershot)` for case 5.
pub(super) fn knit_to_lcl_decision(
    entry: &LedgerHeaderHistoryEntry,
    lcl: &HeaderSnapshot,
) -> Result<KnitDecision> {
    let entry_seq = entry.header.ledger_seq;
    let lcl_seq = lcl.header.ledger_seq;

    // Case 1: entry.seq + 1 < lcl.seq — well before LCL, drop silently.
    if entry_seq.saturating_add(1) < lcl_seq {
        debug!(entry_seq, lcl_seq, "Knit: case 1 (skip-old)");
        return Ok(KnitDecision::Skip);
    }

    // Case 2: entry.seq + 1 == lcl.seq — must match lcl.previousLedgerHash.
    if entry_seq.saturating_add(1) == lcl_seq {
        let expected = Hash256::from(lcl.header.previous_ledger_hash.clone());
        let actual = Hash256::from(entry.hash.clone());
        if expected != actual {
            return Err(HistoryError::KnitLclPredecessorHashMismatch {
                ledger: entry_seq,
                expected: expected.to_hex(),
                actual: actual.to_hex(),
            });
        }
        debug!(entry_seq, "Knit: case 2 (LCL predecessor) hash matches");
        return Ok(KnitDecision::Skip);
    }

    // Case 3: entry.seq == lcl.seq — must match lcl.hash.
    if entry_seq == lcl_seq {
        let actual = Hash256::from(entry.hash.clone());
        if actual != lcl.hash {
            return Err(HistoryError::KnitLclHashMismatch {
                expected: lcl.hash.to_hex(),
                actual: actual.to_hex(),
            });
        }
        debug!(entry_seq, "Knit: case 3 (LCL overlap) hash matches");
        return Ok(KnitDecision::Skip);
    }

    // Case 5: entry.seq > lcl.seq + 1 — overshoot (checked before case 4 to
    // match stellar-core's branch order at ApplyCheckpointWork.cpp:246).
    if entry_seq != lcl_seq.saturating_add(1) {
        return Err(HistoryError::KnitOvershot { entry_seq, lcl_seq });
    }

    // Case 4: entry.seq == lcl.seq + 1 — entry.header.previousLedgerHash
    // must match lcl.hash.
    let entry_prev = Hash256::from(entry.header.previous_ledger_hash.clone());
    if entry_prev != lcl.hash {
        return Err(HistoryError::KnitCurrentLedgerPrevHashMismatch {
            ledger: entry_seq,
            expected: lcl.hash.to_hex(),
            actual: entry_prev.to_hex(),
        });
    }
    debug!(entry_seq, "Knit: case 4 (apply) prev-hash matches");
    Ok(KnitDecision::Apply)
}

/// Published-checkpoint precondition gate for the knit/replay path (#2931).
///
/// Mirrors stellar-core's `GetHistoryArchiveStateWork` retry-until-published
/// precondition (`CatchupWork.cpp` HAS fetch with retries; covered by
/// `LedgerApplyManagerImpl`'s wait for the following checkpoint) that
/// henyey's cloned-local `ReplayOnly` fast path optimized away. stellar-core
/// has no such fast path and always fetches the HAS before applying a
/// checkpoint, so a target whose covering checkpoint is not yet published is
/// never attempted upstream.
///
/// The gate keys off **archive HAS truth** (`has_current_ledger`, the
/// archive's published frontier), NOT the local externalized counter — using
/// the local counter would reintroduce the stale-counter deadlock that the
/// original `catchup_impl` guard removal (proceeding anyway) addressed.
///
/// Returns:
/// - `Ok(())` when `has_current_ledger >= checkpoint_containing(target)` (the
///   covering checkpoint is published; proceed to knit/replay).
/// - `Err(CheckpointNotYetPublished { .. })` (transient) otherwise, so the
///   attempt is retried with backoff instead of triggering a FATAL
///   state-wipe on a knit-to-LCL mismatch against a stale archive header.
///
/// HAS **unreachability** (network/404) is handled by the caller as a
/// transient download error and never reaches this gate — keeping the error
/// taxonomy distinct (HAS-unreachable vs HAS-behind).
pub(super) fn checkpoint_publication_gate(has_current_ledger: u32, target: u32) -> Result<()> {
    let target_checkpoint = crate::checkpoint::checkpoint_containing(target);
    if has_current_ledger < target_checkpoint {
        return Err(HistoryError::CheckpointNotYetPublished {
            target,
            has_current: has_current_ledger,
        });
    }
    Ok(())
}

/// Maximum number of retry attempts for the download-and-replay pipeline.
///
/// Matches stellar-core's `BasicWork::RETRY_A_FEW` used by
/// `DownloadApplyTxsWork` (a `BatchWork` subclass). On each retry the
/// replay start is recalculated from the current LCL, so partial
/// progress is preserved (mirroring stellar-core's `resetIter()`).
pub(super) const REPLAY_RETRY_COUNT: u32 = 5;

/// Base delay in milliseconds for exponential backoff between retries.
const RETRY_BASE_DELAY_MS: u64 = 200;

/// Maximum number of bit-shifts applied to the base delay (caps at 200 * 2^4 = 3200ms).
const RETRY_MAX_BACKOFF_SHIFT: u32 = 4;

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
    /// Apply the §11.2 5-case decision matrix to the knit-prefix entries
    /// (entries at or below LCL drawn from the same checkpoint file as
    /// LCL+1) and to the first apply entry. Returns the apply entries that
    /// must be replayed (i.e. `Apply`-classified entries), in the same
    /// order as `apply_data`.
    ///
    /// `knit_entries` carries the raw `LedgerHeaderHistoryEntry` records
    /// (as found in the archive checkpoint file) for ledgers in
    /// `[knit_start, lcl_seq]`. They are validated against `lcl` and
    /// dropped from replay; mismatches surface as the case-specific fatal
    /// variants on [`HistoryError`].
    ///
    /// `apply_data` carries the per-ledger `LedgerData` for ledgers in
    /// `[lcl_seq + 1, target]`. Its first entry is checked for cases 4 and
    /// 5 (apply-link to LCL or overshoot). Remaining entries are validated
    /// by the chain check downstream.
    pub(super) fn verify_knit_to_lcl(
        &self,
        knit_entries: &[LedgerHeaderHistoryEntry],
        apply_data: &[LedgerData],
        lcl: &HeaderSnapshot,
    ) -> Result<()> {
        for entry in knit_entries {
            let decision = knit_to_lcl_decision(entry, lcl)?;
            debug_assert_eq!(
                decision,
                KnitDecision::Skip,
                "knit-prefix entries must classify as Skip"
            );
        }
        if let Some(first) = apply_data.first() {
            let header = first.header().clone();
            // `knit_to_lcl_decision` only reads `header.previous_ledger_hash` on
            // the case-4 (Apply) branch exercised here, so the entry hash is
            // unused. Use a zero placeholder rather than recomputing the header
            // hash for a synthetic entry that we discard immediately.
            let virtual_entry = LedgerHeaderHistoryEntry {
                hash: stellar_xdr::curr::Hash([0; 32]),
                header,
                ext: Default::default(),
            };
            let decision = knit_to_lcl_decision(&virtual_entry, lcl)?;
            debug_assert_eq!(
                decision,
                KnitDecision::Apply,
                "first apply entry must classify as Apply"
            );
        }
        info!(
            knit_entries = knit_entries.len(),
            apply_entries = apply_data.len(),
            "Knit-to-LCL decision matrix validated"
        );
        Ok(())
    }

    /// Verify the downloaded ledger data using reverse-walk chain verification (§9.2–§9.5).
    pub(super) fn verify_downloaded_data(
        &self,
        ledger_data: &[LedgerData],
        lcl_snapshot: &HeaderSnapshot,
    ) -> Result<()> {
        if ledger_data.is_empty() {
            return Ok(());
        }

        // Extract headers for chain verification
        let headers: Vec<_> = ledger_data.iter().map(|d| d.header().clone()).collect();

        // Skip header chain and trust anchor verification when verify_header_chain
        // is false. This allows synthetic tests to bypass chain integrity checks.
        if self.replay_config.verify_header_chain {
            // Reverse-walk verification (§9.2–§9.5): processes checkpoints
            // from highest to lowest, threading trust from the top anchor.
            // Individual header integrity was already verified during download
            // (per-checkpoint verify_header_chain_from_entries).
            let trust_source = match &self.trusted_scp_anchor {
                Some((seq, hash)) => verify::TrustSource::Scp {
                    seq: *seq,
                    hash: *hash,
                },
                None => verify::TrustSource::None,
            };
            let config = verify::ReverseWalkConfig {
                trust_source,
                lcl: Some((lcl_snapshot.header.ledger_seq, lcl_snapshot.hash)),
                max_supported_version: henyey_common::protocol::CURRENT_LEDGER_PROTOCOL_VERSION,
                min_supported_version: henyey_common::protocol::MIN_LEDGER_PROTOCOL_VERSION,
            };
            verify::verify_reverse_walk(&headers, &config)?;
        }

        // Verify transaction sets and result sets match header hashes.
        // tx_set is always available (synthesized for absent entries), matching
        // stellar-core's unconditional verification (ApplyCheckpointWork.cpp:280).
        // tx_result_set verification is skipped for absent entries.
        for data in ledger_data {
            let tx_set = data.tx_set();
            verify::verify_tx_set(data.header(), &tx_set)?;

            if let Some(result_entry) = data.tx_result_entry() {
                let xdr = result_entry
                    .tx_result_set
                    .to_xdr(stellar_xdr::curr::Limits::none())
                    .map_err(|e| {
                        HistoryError::CatchupFailed(format!(
                            "Failed to serialize tx result set for ledger {}: {}",
                            data.header().ledger_seq,
                            e
                        ))
                    })?;
                verify::verify_tx_result_set(data.header(), &xdr)?;
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
    ///
    /// Stage E instrumentation: emits
    /// `stellar_history_apply_ledger_chain_{success,failure}_total` exactly
    /// once per *outer* call, on terminal outcome (after all retries). Per-
    /// attempt verify failures are surfaced via the separate
    /// `stellar_history_verify_ledger_chain_*` counters in
    /// `download_verify_and_replay_once`.
    pub(super) async fn download_verify_and_replay_with_retry(
        &mut self,
        target: u32,
        ledger_manager: &LedgerManager,
    ) -> Result<(HeaderSnapshot, u32)> {
        let last_archive = self
            .archives
            .last()
            .map(|a| a.name().to_owned())
            .unwrap_or_default();
        let result = self
            .download_verify_and_replay_with_retry_inner(target, ledger_manager)
            .await;
        match &result {
            Ok((_, _, archive_name)) => {
                metrics::counter!(
                    "stellar_history_apply_ledger_chain_success_total",
                    "archive" => archive_name.clone(),
                )
                .increment(1);
            }
            Err(_) => {
                // All retries exhausted — attribute to the last attempted archive.
                metrics::counter!(
                    "stellar_history_apply_ledger_chain_failure_total",
                    "archive" => last_archive,
                )
                .increment(1);
            }
        }
        result.map(|(snap, count, _)| (snap, count))
    }

    async fn download_verify_and_replay_with_retry_inner(
        &mut self,
        target: u32,
        ledger_manager: &LedgerManager,
    ) -> Result<(HeaderSnapshot, u32, String)> {
        let mut last_error: Option<HistoryError> = None;

        for attempt in 0..=REPLAY_RETRY_COUNT {
            if attempt > 0 {
                let delay_ms =
                    RETRY_BASE_DELAY_MS * (1 << (attempt - 1).min(RETRY_MAX_BACKOFF_SHIFT));
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
            // Use a single atomic snapshot to avoid split reads.
            let snap = ledger_manager.header_snapshot();
            let current_lcl = snap.header.ledger_seq;
            let replay_first = current_lcl + 1;

            if replay_first > target {
                // Already past target — previous partial replay succeeded fully.
                // No download occurred this iteration, so use "none".
                let ledgers_applied = target.saturating_sub(current_lcl);
                return Ok((snap, ledgers_applied, "none".to_owned()));
            }

            // Download from the checkpoint containing replay_first - 1 (the LCL).
            let download_from = current_lcl;
            let lcl = LclContext::from(&snap);

            match self
                .download_verify_and_replay_once(download_from, target, lcl, ledger_manager)
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
        lcl: LclContext,
        ledger_manager: &LedgerManager,
    ) -> Result<(HeaderSnapshot, u32, String)> {
        use henyey_common::NetworkId;

        // #2931: published-checkpoint precondition gate. Runs BEFORE any
        // checkpoint-file / LCL-header fetch in this attempt so the stale
        // archive header at the LCL (refetched in download_ledger_data) can
        // never surface a knit-to-LCL mismatch against an unpublished target.
        //
        // Fetch the HAS for the target's covering checkpoint and require the
        // archive's published frontier (currentLedger) to cover it. HAS
        // unreachability (network/404) maps to a transient download/
        // CatchupFailed error here (distinct taxonomy); a reachable HAS whose
        // currentLedger is behind the target checkpoint yields the transient
        // CheckpointNotYetPublished so the attempt is retried, not wiped.
        let target_checkpoint = crate::checkpoint::checkpoint_containing(target);
        let (target_has, gate_archive) = self.download_has(target_checkpoint).await?;
        checkpoint_publication_gate(target_has.current_ledger(), target)?;

        // #2940: pin every ledger-data download in this attempt to the exact
        // archive that served `target_has` and passed the publication gate.
        // This guarantees the archive whose published frontier satisfied the
        // gate is the same archive that supplies the LCL header and checkpoint
        // payloads — closing the cross-archive bypass where a different stale-
        // but-serving archive could supply ledger data that never satisfied the
        // gate (#2931 knit-to-LCL / state-wipe path under archive asymmetry).
        //
        // The pin is re-derived on every call to this method (i.e. per replay
        // attempt) — never cached across attempts — since `download_has`
        // re-scans archives from index 0 each time. A pinned download that
        // fails returns an error for this attempt (no intra-attempt cross-
        // archive fallback); the outer retry loop then re-runs the whole
        // attempt, re-running the gate and re-selecting the archive, so a down
        // archive is still rotated at the attempt boundary.

        // Download ledger data for replay
        self.update_progress(
            CatchupStatus::DownloadingLedgers,
            4,
            "Downloading ledger data",
        );
        let (ledger_data, knit_entries, archive_name) = self
            .download_ledger_data(download_from, target, lcl, Some(&gate_archive))
            .await?;

        // CATCHUP_SPEC §11.2: apply the 5-case knit-to-LCL decision matrix
        // before chain verification. This catches case-2/3/4/5 hash and
        // sequencing failures with their specific fatal error variants
        // (mirroring stellar-core ApplyCheckpointWork::getNextLedgerCloseData())
        // rather than letting them surface as generic chain-link errors.
        let lcl_snapshot = ledger_manager.header_snapshot();
        self.verify_knit_to_lcl(&knit_entries, &ledger_data, &lcl_snapshot)?;

        // Verify the header chain.
        //
        // Stage E instrumentation: counts each call to `verify_downloaded_data`
        // (one per replay attempt). This is independent from the outer
        // `apply_ledger_chain_*` counters: a single outer success can include
        // multiple verify failures from prior attempts.
        self.update_progress(CatchupStatus::Verifying, 5, "Verifying header chain");
        match self.verify_downloaded_data(&ledger_data, &lcl_snapshot) {
            Ok(()) => {
                metrics::counter!(
                    "stellar_history_verify_ledger_chain_success_total",
                    "archive" => archive_name.clone(),
                )
                .increment(1);
            }
            Err(e) => {
                metrics::counter!(
                    "stellar_history_verify_ledger_chain_failure_total",
                    "archive" => archive_name,
                )
                .increment(1);
                return Err(e);
            }
        }

        // Replay ledgers via close_ledger
        self.update_progress(CatchupStatus::Replaying, 6, "Replaying ledgers");
        self.replay_via_close_ledger(ledger_manager, &ledger_data)
            .await?;

        let network_id = NetworkId(ledger_manager.network_id().0);
        self.persist_ledger_history(&ledger_data, &network_id)?;

        let snap = ledger_manager.header_snapshot();
        let ledgers_applied = snap.header.ledger_seq.saturating_sub(download_from);

        Ok((snap, ledgers_applied, archive_name))
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
            self.progress.current_ledger = data.header().ledger_seq;

            // Apply publish queue backpressure if enabled (offline catchup).
            if self.replay_config.wait_for_publish {
                self.wait_for_publish_queue(&mut pq_fell_behind).await?;
            }

            // Decode upgrades from the header's scp_value.upgrades
            let upgrades = decode_upgrades_from_header(data.header());

            // Compute expected header hash from archive header for pre-commit validation.
            let expected_hash = if self.replay_config.verify_header_hash {
                Some(
                    henyey_ledger::compute_header_hash(data.header()).map_err(|e| {
                        HistoryError::CatchupFailed(format!(
                            "Failed to compute header hash for ledger {}: {}",
                            data.header().ledger_seq,
                            e
                        ))
                    })?,
                )
            } else {
                None
            };

            let mut close_data = LedgerCloseData::new(
                data.header().ledger_seq,
                data.tx_set(),
                data.header().scp_value.close_time.0,
                ledger_manager.current_header_hash(),
            )
            .with_stellar_value_ext(data.header().scp_value.ext.clone())
            .with_upgrades(upgrades);

            if let Some(hash) = expected_hash {
                close_data = close_data.with_expected_header_hash(hash);
            }

            let result = ledger_manager
                .close_ledger(close_data, None)
                .map_err(|e| match e {
                    henyey_ledger::LedgerError::HashMismatch { expected, actual } => {
                        HistoryError::ReplayHashMismatch {
                            ledger: data.header().ledger_seq,
                            expected,
                            actual,
                        }
                    }
                    other => HistoryError::CatchupFailed(format!(
                        "close_ledger failed at ledger {}: {}",
                        data.header().ledger_seq,
                        other
                    )),
                })?;

            // Emit metadata to SQLite and external consumers (e.g., stellar-rpc's
            // meta pipe in bounded replay mode: `catchup --metadata-output-stream fd:3`).
            if let Some(meta) = result.meta {
                self.emit_meta(data.header().ledger_seq, meta);
            }

            debug!(
                "Replayed ledger {}/{} via close_ledger: seq={}",
                i + 1,
                total,
                data.header().ledger_seq
            );

            // Yield to the tokio runtime between close_ledger calls so that
            // the event loop can process SCP messages, heartbeats, etc.
            tokio::task::yield_now().await;
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
    use henyey_common::Hash256;

    /// Construct a synthetic [`HeaderSnapshot`] for the §11.2 knit tests.
    /// `lcl_seq` is the LCL ledger sequence, `lcl_hash` is the LCL's own
    /// hash, and `lcl_prev_hash` is what the LCL header's
    /// `previous_ledger_hash` field carries (i.e. the hash of LCL-1).
    fn make_test_lcl(lcl_seq: u32, lcl_hash: Hash256, lcl_prev_hash: Hash256) -> HeaderSnapshot {
        use stellar_xdr::curr::LedgerHeader;
        let mut header = LedgerHeader::default();
        header.ledger_seq = lcl_seq;
        header.previous_ledger_hash = stellar_xdr::curr::Hash(lcl_prev_hash.0);
        HeaderSnapshot {
            header,
            hash: lcl_hash,
            soroban_network_info: None,
        }
    }

    /// Construct a synthetic [`LedgerHeaderHistoryEntry`].
    /// `entry_hash` is the entry's own `hash` field (case 2/3 check) and
    /// `entry_prev` populates `header.previous_ledger_hash` (case 4 check).
    fn make_test_entry(
        seq: u32,
        entry_hash: Hash256,
        entry_prev: Hash256,
    ) -> stellar_xdr::curr::LedgerHeaderHistoryEntry {
        use stellar_xdr::curr::{LedgerHeader, LedgerHeaderHistoryEntry};
        let mut header = LedgerHeader::default();
        header.ledger_seq = seq;
        header.previous_ledger_hash = stellar_xdr::curr::Hash(entry_prev.0);
        LedgerHeaderHistoryEntry {
            hash: stellar_xdr::curr::Hash(entry_hash.0),
            header,
            ext: Default::default(),
        }
    }

    fn h(byte: u8) -> Hash256 {
        Hash256::from_bytes([byte; 32])
    }

    #[test]
    fn test_knit_case_1_skip_old() {
        let lcl = make_test_lcl(100, h(0x10), h(0x09));
        let entry = make_test_entry(80, h(0x99), h(0x88)); // hashes irrelevant
        assert_eq!(
            knit_to_lcl_decision(&entry, &lcl).unwrap(),
            KnitDecision::Skip
        );
    }

    #[test]
    fn test_knit_case_2_lcl_predecessor_match() {
        let lcl = make_test_lcl(100, h(0x10), h(0x09));
        // Entry at LCL-1 whose own hash equals lcl.previousLedgerHash.
        let entry = make_test_entry(99, h(0x09), h(0x08));
        assert_eq!(
            knit_to_lcl_decision(&entry, &lcl).unwrap(),
            KnitDecision::Skip
        );
    }

    #[test]
    fn test_knit_case_2_lcl_predecessor_mismatch() {
        let lcl = make_test_lcl(100, h(0x10), h(0x09));
        let entry = make_test_entry(99, h(0xAA), h(0x08));
        let err = knit_to_lcl_decision(&entry, &lcl).unwrap_err();
        match err {
            HistoryError::KnitLclPredecessorHashMismatch { ledger, .. } => {
                assert_eq!(ledger, 99);
            }
            other => panic!("expected KnitLclPredecessorHashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_knit_case_3_lcl_overlap_match() {
        let lcl = make_test_lcl(100, h(0x10), h(0x09));
        // Entry at LCL whose own hash matches lcl.hash.
        let entry = make_test_entry(100, h(0x10), h(0x09));
        assert_eq!(
            knit_to_lcl_decision(&entry, &lcl).unwrap(),
            KnitDecision::Skip
        );
    }

    #[test]
    fn test_knit_case_3_lcl_overlap_mismatch() {
        let lcl = make_test_lcl(100, h(0x10), h(0x09));
        let entry = make_test_entry(100, h(0xBB), h(0x09));
        let err = knit_to_lcl_decision(&entry, &lcl).unwrap_err();
        assert!(matches!(err, HistoryError::KnitLclHashMismatch { .. }));
    }

    #[test]
    fn test_knit_case_4_apply_match() {
        let lcl = make_test_lcl(100, h(0x10), h(0x09));
        // LCL+1 whose previousLedgerHash matches lcl.hash.
        let entry = make_test_entry(101, h(0xCC), h(0x10));
        assert_eq!(
            knit_to_lcl_decision(&entry, &lcl).unwrap(),
            KnitDecision::Apply
        );
    }

    #[test]
    fn test_knit_case_4_apply_prev_hash_mismatch() {
        let lcl = make_test_lcl(100, h(0x10), h(0x09));
        // LCL+1 but previousLedgerHash != lcl.hash.
        let entry = make_test_entry(101, h(0xCC), h(0xEE));
        let err = knit_to_lcl_decision(&entry, &lcl).unwrap_err();
        match err {
            HistoryError::KnitCurrentLedgerPrevHashMismatch { ledger, .. } => {
                assert_eq!(ledger, 101);
            }
            other => panic!("expected KnitCurrentLedgerPrevHashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_knit_case_5_overshoot() {
        let lcl = make_test_lcl(100, h(0x10), h(0x09));
        let entry = make_test_entry(105, h(0xCC), h(0xDD));
        let err = knit_to_lcl_decision(&entry, &lcl).unwrap_err();
        match err {
            HistoryError::KnitOvershot { entry_seq, lcl_seq } => {
                assert_eq!(entry_seq, 105);
                assert_eq!(lcl_seq, 100);
            }
            other => panic!("expected KnitOvershot, got {other:?}"),
        }
    }

    #[test]
    fn test_knit_at_genesis() {
        // Synthetic pre-genesis LCL (seq=0, zero hashes), as used by henyey
        // when the local LedgerManager is at synthetic genesis.
        let lcl = make_test_lcl(0, Hash256::ZERO, Hash256::ZERO);
        // Entry at ledger 1 with previousLedgerHash == lcl.hash (zero):
        // classifies as case 4 (apply).
        let entry = make_test_entry(1, h(0xAB), Hash256::ZERO);
        assert_eq!(
            knit_to_lcl_decision(&entry, &lcl).unwrap(),
            KnitDecision::Apply
        );
        // Entry at ledger 2 → overshoot.
        let entry2 = make_test_entry(2, h(0xCD), Hash256::ZERO);
        assert!(matches!(
            knit_to_lcl_decision(&entry2, &lcl).unwrap_err(),
            HistoryError::KnitOvershot {
                entry_seq: 2,
                lcl_seq: 0
            }
        ));
    }

    #[test]
    fn test_knit_lcl_at_checkpoint_boundary() {
        // This test exercises the pure decision function only — it has no
        // checkpoint awareness, so any LCL seq is a valid input. LCL=127
        // is chosen here for convenience; we are NOT asserting anything
        // about which checkpoint file LCL-1 actually lives in.
        //
        // Separate note about the surrounding download path (not exercised
        // here): with checkpoint frequency 64, LCL-1 is only visible in the
        // LCL+1 download when LCL and LCL-1 share the LCL+1 checkpoint
        // file. That holds for e.g. LCL=100 ([64..127], LCL+1=101 still in
        // the same checkpoint), but NOT for LCL=127 — there LCL=127 is the
        // last ledger of checkpoint K=127 ([64..127]) and LCL+1=128 starts
        // a new checkpoint K=191 ([128..191]), so neither LCL nor LCL-1
        // appear in the LCL+1 download file. The decision function still
        // returns Skip for the case 2 input regardless.
        let lcl = make_test_lcl(127, h(0x10), h(0x09));
        let entry_at_lcl_minus_1 = make_test_entry(126, h(0x09), h(0x08));
        assert_eq!(
            knit_to_lcl_decision(&entry_at_lcl_minus_1, &lcl).unwrap(),
            KnitDecision::Skip
        );
        // When LCL is the FIRST ledger of its checkpoint (seq == 64, 128, ...),
        // LCL-1 lives in a prior checkpoint and is NOT downloaded — the knit
        // pass simply doesn't see it. We exercise only LCL itself (case 3) and
        // LCL+1 (case 4).
        let lcl_boundary = make_test_lcl(64, h(0x20), h(0x1F));
        let entry_at_lcl = make_test_entry(64, h(0x20), h(0x1F));
        assert_eq!(
            knit_to_lcl_decision(&entry_at_lcl, &lcl_boundary).unwrap(),
            KnitDecision::Skip
        );
        let entry_at_lcl_plus_1 = make_test_entry(65, h(0xAB), h(0x20));
        assert_eq!(
            knit_to_lcl_decision(&entry_at_lcl_plus_1, &lcl_boundary).unwrap(),
            KnitDecision::Apply
        );
    }

    #[test]
    fn test_knit_after_retry_advances_lcl() {
        // Simulate a partial-progress retry: LCL has advanced past the
        // earliest entry in the original batch. Older entries must
        // classify as case 1 (skip-old), not be re-applied.
        let lcl = make_test_lcl(110, h(0xFE), h(0xFD));
        let old_entry = make_test_entry(105, h(0x11), h(0x10));
        assert_eq!(
            knit_to_lcl_decision(&old_entry, &lcl).unwrap(),
            KnitDecision::Skip
        );
        // And the entry at LCL still classifies as case 3 (overlap) and
        // requires the hash to match — protecting against an attacker
        // replaying older but tampered history.
        let entry_at_lcl_tampered = make_test_entry(110, h(0xBA), h(0xFD));
        assert!(matches!(
            knit_to_lcl_decision(&entry_at_lcl_tampered, &lcl).unwrap_err(),
            HistoryError::KnitLclHashMismatch { .. }
        ));
    }

    // ---------------------------------------------------------------
    // Direct tests for the `verify_knit_to_lcl` wrapper (#2737).
    //
    // The 11 `knit_to_lcl_decision` tests above exhaustively cover the
    // §11.2 five-case decision matrix. The tests below target the
    // wrapper's own glue that is *not* covered by those tests:
    //   - the loop over `knit_entries` (multi-iteration, mid-loop `?`),
    //   - the `apply_data.first()` short-circuit on empty input,
    //   - the synthesis of the virtual `LedgerHeaderHistoryEntry` from
    //     `apply_data[0]` for the case-4 (Apply) check.
    // ---------------------------------------------------------------

    /// Returns the `TempDir` guard backing the `BucketManager` together
    /// with the manager. The `TempDir` is returned **first** so callers
    /// destructure as `let (_tmp_dir, manager) = ...`: Rust drops local
    /// bindings in reverse declaration order, so binding `manager`
    /// second guarantees it (and any file handles its `BucketManager`
    /// holds) is dropped before the `TempDir` removes the directory.
    /// The caller must keep `_tmp_dir` alive for the duration of the
    /// test so the bucket directory is deleted on drop rather than
    /// leaked under the system temp location.
    fn make_test_catchup_manager() -> (tempfile::TempDir, CatchupManager) {
        use henyey_bucket::BucketManager;
        use henyey_db::Database;

        let db = Database::open_in_memory().expect("in-memory db");
        let tmp_dir = tempfile::tempdir().expect("temp dir");
        let bucket_manager =
            BucketManager::new(tmp_dir.path().to_path_buf()).expect("bucket manager");
        let archive = crate::HistoryArchive::new("https://example.com").expect("archive");
        (
            tmp_dir,
            CatchupManager::new(vec![archive], bucket_manager, db),
        )
    }

    /// Build a `LedgerData` for ledger `seq` whose
    /// `header.previous_ledger_hash` is `prev_hash`. The accompanying
    /// `LclContext` carries the same hash as its `lcl_hash`, so the
    /// `(None, None)` arm of `LedgerData::new` (which validates
    /// `header.previous_ledger_hash == lcl.lcl_hash()`) succeeds.
    ///
    /// `.expect("valid LedgerData")` is intentional: a misconstructed
    /// helper surfaces as a clear panic at construction time rather
    /// than as a confusing `HistoryError::VerificationFailed` bubbling
    /// out of `verify_knit_to_lcl` later.
    fn make_apply_ledger_data(seq: u32, prev_hash: Hash256) -> LedgerData {
        use henyey_common::protocol::LclContext;
        use stellar_xdr::curr::LedgerHeader;

        let mut header = LedgerHeader::default();
        header.ledger_seq = seq;
        header.previous_ledger_hash = stellar_xdr::curr::Hash(prev_hash.0);

        let lcl = LclContext::new(0, prev_hash);
        LedgerData::new(header, None, None, &lcl).expect("valid LedgerData")
    }

    #[test]
    fn test_verify_knit_to_lcl_happy_path_mixed() {
        // LCL = 100, lcl_hash = H100, previous_ledger_hash = H99.
        let lcl = make_test_lcl(100, h(0x10), h(0x09));
        let (_tmp_dir, manager) = make_test_catchup_manager();

        // Three knit entries spanning cases 1/2/3 — each must classify
        // as Skip inside the loop, exercising 3 iterations of the body.
        let knit_entries = vec![
            make_test_entry(98, h(0x99), h(0x88)), // case 1 (skip-old): hashes irrelevant
            make_test_entry(99, h(0x09), h(0x08)), // case 2: entry.hash == lcl.previous_ledger_hash
            make_test_entry(100, h(0x10), h(0x09)), // case 3: entry.hash == lcl.hash
        ];

        // apply_data[0] is the one that exercises the wrapper's
        // virtual-entry synthesis (case 4): its `previous_ledger_hash`
        // must equal `lcl.hash`. apply_data[1] is included to confirm
        // the wrapper only consults `.first()`; its own previous-hash
        // chains off `compute_header_hash(apply_data[0].header)`.
        //
        // Note: the synthesized virtual entry's *own* `hash` field is
        // irrelevant on the case-4 branch — `knit_to_lcl_decision`
        // only reads `header.previous_ledger_hash` for Apply.
        let ledger_101 = make_apply_ledger_data(101, h(0x10));
        let hash_101 = henyey_ledger::compute_header_hash(ledger_101.header())
            .expect("compute hash for ledger 101");
        let ledger_102 = make_apply_ledger_data(102, hash_101);
        let apply_data = vec![ledger_101, ledger_102];

        manager
            .verify_knit_to_lcl(&knit_entries, &apply_data, &lcl)
            .expect("happy path must succeed");
    }

    #[test]
    fn test_verify_knit_to_lcl_tampered_knit_entry_returns_fatal() {
        // LCL = 100, lcl_hash = H100, previous_ledger_hash = H99.
        let lcl = make_test_lcl(100, h(0x10), h(0x09));
        let (_tmp_dir, manager) = make_test_catchup_manager();
        // apply_data is irrelevant — the loop must error out before
        // we reach the `.first()` branch.
        let apply_data = vec![make_apply_ledger_data(101, h(0x10))];

        // (a) Mid-loop failure on the *second* iteration: case 2 passes,
        // case 3 hash is tampered. This proves `?` propagates from
        // inside the loop, not just from the first iteration.
        {
            let knit_entries = vec![
                make_test_entry(99, h(0x09), h(0x08)),  // case 2 ok
                make_test_entry(100, h(0xBB), h(0x09)), // case 3 mismatch (entry.hash != lcl.hash)
            ];
            let err = manager
                .verify_knit_to_lcl(&knit_entries, &apply_data, &lcl)
                .expect_err("tampered case-3 entry must produce fatal");
            assert!(
                matches!(err, HistoryError::KnitLclHashMismatch { .. }),
                "expected KnitLclHashMismatch, got {err:?}",
            );
        }

        // (b) First-iteration failure with a *different* variant —
        // confirms the loop does not remap or swallow distinct error
        // variants from `knit_to_lcl_decision`.
        {
            let knit_entries = vec![
                make_test_entry(99, h(0xAA), h(0x08)), // case 2 mismatch (entry.hash != lcl.previous_ledger_hash)
            ];
            let err = manager
                .verify_knit_to_lcl(&knit_entries, &apply_data, &lcl)
                .expect_err("tampered case-2 entry must produce fatal");
            assert!(
                matches!(err, HistoryError::KnitLclPredecessorHashMismatch { .. }),
                "expected KnitLclPredecessorHashMismatch, got {err:?}",
            );
        }
    }

    #[test]
    fn test_verify_knit_to_lcl_empty_apply_data_noop() {
        let lcl = make_test_lcl(100, h(0x10), h(0x09));
        let (_tmp_dir, manager) = make_test_catchup_manager();

        // No knit entries, no apply data: pure no-op, must Ok.
        manager
            .verify_knit_to_lcl(&[], &[], &lcl)
            .expect("empty inputs must be a no-op");

        // Skip-only knit entries with empty apply data: the loop runs
        // but the `apply_data.first()` branch is skipped — no panic,
        // no error.
        let knit_entries = vec![
            make_test_entry(99, h(0x09), h(0x08)),  // case 2
            make_test_entry(100, h(0x10), h(0x09)), // case 3
        ];
        manager
            .verify_knit_to_lcl(&knit_entries, &[], &lcl)
            .expect("skip-only entries with empty apply_data must succeed");
    }

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
            crate::error::TxSetHashMismatchInfo::new(
                Hash256::ZERO,
                Hash256::ZERO,
                0,
                Hash256::ZERO,
                Hash256::ZERO,
                "classic",
            )
            .into_error(100),
            HistoryError::InvalidSequence {
                expected: 100,
                got: 200,
            },
            HistoryError::CorruptHeader {
                ledger: 100,
                detail: "bad encoding".to_string(),
            },
            HistoryError::Ledger(henyey_ledger::LedgerError::HashMismatch {
                expected: "abc".into(),
                actual: "def".into(),
            }),
            HistoryError::ReplayHashMismatch {
                ledger: 100,
                expected: "abc".into(),
                actual: "def".into(),
            },
            HistoryError::KnitLclPredecessorHashMismatch {
                ledger: 99,
                expected: "abc".into(),
                actual: "def".into(),
            },
            HistoryError::KnitLclHashMismatch {
                expected: "abc".into(),
                actual: "def".into(),
            },
            HistoryError::KnitCurrentLedgerPrevHashMismatch {
                ledger: 101,
                expected: "abc".into(),
                actual: "def".into(),
            },
            HistoryError::KnitOvershot {
                entry_seq: 105,
                lcl_seq: 100,
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

    /// Stage E: pin the metric literals emitted from this module so a typo
    /// can't silently detach this crate from the central catalog.
    #[test]
    fn test_stage_e_replay_metric_literals_present() {
        let src = include_str!("replay.rs");
        for literal in &[
            "\"stellar_history_apply_ledger_chain_success_total\"",
            "\"stellar_history_apply_ledger_chain_failure_total\"",
            "\"stellar_history_verify_ledger_chain_success_total\"",
            "\"stellar_history_verify_ledger_chain_failure_total\"",
        ] {
            assert!(
                src.contains(literal),
                "expected metric literal {literal} in catchup/replay.rs",
            );
        }
    }

    /// Stage E: verify and apply counters must carry the `"archive"` label.
    #[test]
    fn test_stage_e_replay_archive_label_present() {
        let src = include_str!("replay.rs");
        let main_code = src.split("#[cfg(test)]").next().unwrap_or(src);
        for metric in &[
            "stellar_history_apply_ledger_chain_success_total",
            "stellar_history_apply_ledger_chain_failure_total",
            "stellar_history_verify_ledger_chain_success_total",
            "stellar_history_verify_ledger_chain_failure_total",
        ] {
            let mut search_from = 0;
            let mut found_any = false;
            while let Some(rel_idx) = main_code[search_from..].find(metric) {
                found_any = true;
                let idx = search_from + rel_idx;
                let window = &main_code[idx..std::cmp::min(idx + 200, main_code.len())];
                assert!(
                    window.contains("\"archive\""),
                    "metric {metric} missing \"archive\" label at byte offset {idx} \
                     in catchup/replay.rs",
                );
                search_from = idx + metric.len();
            }
            assert!(found_any, "metric {metric} not found in catchup/replay.rs",);
        }
    }

    // ================================================================
    // §9.1 + INV-C5: Trusted SCP anchor configuration tests (#2830)
    // ================================================================

    /// Build a valid header chain and matching `LedgerData` entries.
    /// Returns (ledger_data_vec, hash_of_header_before_chain) where the
    /// hash before the chain is the `previous_ledger_hash` of the first header.
    fn make_test_ledger_data_chain(
        start_seq: u32,
        count: u32,
    ) -> (Vec<super::LedgerData>, Hash256) {
        use crate::verify::compute_header_hash;
        use henyey_common::protocol::LclContext;
        use stellar_xdr::curr::{Hash, StellarValue, TimePoint, VecM};

        let mut entries = Vec::with_capacity(count as usize);
        let mut prev_hash = Hash256::ZERO;
        let genesis_hash = prev_hash;

        for i in 0..count {
            let seq = start_seq + i;
            let header = LedgerHeader {
                ledger_version: 25,
                previous_ledger_hash: prev_hash.into(),
                scp_value: StellarValue {
                    tx_set_hash: Hash([0u8; 32]),
                    close_time: TimePoint(0),
                    upgrades: VecM::default(),
                    ext: stellar_xdr::curr::StellarValueExt::Basic,
                },
                tx_set_result_hash: Hash([0u8; 32]),
                bucket_list_hash: Hash([0u8; 32]),
                ledger_seq: seq,
                total_coins: 0,
                fee_pool: 0,
                inflation_seq: 0,
                id_pool: 0,
                base_fee: 100,
                base_reserve: 5000000,
                max_tx_set_size: 100,
                skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
                ext: stellar_xdr::curr::LedgerHeaderExt::V0,
            };

            let lcl_context = LclContext::new(25, prev_hash);
            let ledger_data =
                super::LedgerData::new(header.clone(), None, None, &lcl_context).unwrap();
            prev_hash = compute_header_hash(&header).unwrap();
            entries.push(ledger_data);
        }

        (entries, genesis_hash)
    }

    /// With TrustSource::Scp (anchor set), LCL disagreement produces
    /// FatalChainDisagreement through the production `verify_downloaded_data` path.
    #[test]
    fn test_verify_downloaded_data_scp_anchor_makes_lcl_disagreement_fatal() {
        use crate::verify::compute_header_hash;

        let tmpdir = tempfile::tempdir().unwrap();
        let bucket_manager =
            henyey_bucket::BucketManager::new(tmpdir.path().to_path_buf()).unwrap();
        let db = henyey_db::Database::open_in_memory().unwrap();
        let archive =
            crate::archive::HistoryArchive::with_name("http://localhost:1234", "test").unwrap();
        let mut manager = super::CatchupManager::new(vec![archive], bucket_manager, db);

        // Enable header chain verification (production default for catchup).
        manager.replay_config.verify_header_chain = true;

        // Build a valid chain of 5 headers starting at seq 10.
        let (ledger_data, _genesis_hash) = make_test_ledger_data_chain(10, 5);

        // Compute the hash of the last header for the SCP trust anchor.
        let last_header = ledger_data.last().unwrap().header();
        let last_hash = compute_header_hash(last_header).unwrap();

        // Set SCP trust anchor at the last header in the chain.
        manager.set_trusted_scp_anchor(last_header.ledger_seq, last_hash);

        // Build a DISAGREEING LCL: seq = 9 (right before chain start), but
        // hash is wrong (doesn't match first header's previous_ledger_hash).
        let wrong_lcl_hash = Hash256::from_bytes([0xBB; 32]);
        let lcl_snapshot = make_test_lcl(9, wrong_lcl_hash, Hash256::ZERO);

        // Call the production path: verify_downloaded_data.
        let err = manager
            .verify_downloaded_data(&ledger_data, &lcl_snapshot)
            .unwrap_err();

        // With TrustSource::Scp, LCL disagreement must be fatal.
        assert!(
            matches!(err, crate::HistoryError::FatalChainDisagreement),
            "expected FatalChainDisagreement with SCP anchor, got: {err}"
        );
    }

    /// Without SCP anchor (TrustSource::None), LCL disagreement produces
    /// InvalidPreviousHash through the production `verify_downloaded_data` path.
    #[test]
    fn test_verify_downloaded_data_no_anchor_makes_lcl_disagreement_non_fatal() {
        let tmpdir = tempfile::tempdir().unwrap();
        let bucket_manager =
            henyey_bucket::BucketManager::new(tmpdir.path().to_path_buf()).unwrap();
        let db = henyey_db::Database::open_in_memory().unwrap();
        let archive =
            crate::archive::HistoryArchive::with_name("http://localhost:1234", "test").unwrap();
        let mut manager = super::CatchupManager::new(vec![archive], bucket_manager, db);

        // Enable header chain verification.
        manager.replay_config.verify_header_chain = true;

        // Build a valid chain of 5 headers starting at seq 10.
        let (ledger_data, _genesis_hash) = make_test_ledger_data_chain(10, 5);

        // Do NOT set any SCP trust anchor (TrustSource::None by default).

        // Build a DISAGREEING LCL.
        let wrong_lcl_hash = Hash256::from_bytes([0xBB; 32]);
        let lcl_snapshot = make_test_lcl(9, wrong_lcl_hash, Hash256::ZERO);

        // Call the production path: verify_downloaded_data.
        let err = manager
            .verify_downloaded_data(&ledger_data, &lcl_snapshot)
            .unwrap_err();

        // Without SCP anchor, LCL disagreement is treated as a broken chain
        // (InvalidPreviousHash) — retriable, not fatal.
        assert!(
            matches!(err, crate::HistoryError::InvalidPreviousHash { .. }),
            "expected InvalidPreviousHash without SCP anchor, got: {err}"
        );
    }

    // ---------------------------------------------------------------
    // #2931: published-checkpoint precondition gate.
    //
    // Restores the upstream HAS-publication gate (stellar-core
    // `GetHistoryArchiveStateWork` retry-until-published) that henyey's
    // cloned-local ReplayOnly fast path omitted. The gate keys off archive
    // HAS truth (`currentLedger`), NOT the local externalized counter.
    // ---------------------------------------------------------------

    #[test]
    fn test_knit_lcl_mismatch_unpublished_checkpoint_is_transient() {
        // Mirrors the #2931 crash scenario: a ReplayOnly attempt targets
        // ledgers in checkpoint 62845439 while the archive's published
        // frontier (HAS currentLedger) is still on the prior checkpoint.
        //
        // The archive's checkpoint header at the LCL diverges from the
        // (correct) local LCL — on origin/main this surfaces as a fatal
        // KnitLclHashMismatch and wipes state. With the publication gate the
        // attempt is short-circuited as a TRANSIENT CheckpointNotYetPublished
        // BEFORE any knit/replay, so it is retried instead of wiping.
        let target = 62845437; // in checkpoint 62845439
        let target_ckpt = crate::checkpoint::checkpoint_containing(target);
        // Archive has only published through the prior checkpoint.
        let has_current = target_ckpt - crate::checkpoint::checkpoint_frequency();

        let err = checkpoint_publication_gate(has_current, target)
            .expect_err("gate must reject an unpublished target checkpoint");

        // The covering checkpoint is derived from `target` (#2950), so the
        // variant only binds `target`/`has_current`; assert the covering value
        // via the accessor.
        match err {
            HistoryError::CheckpointNotYetPublished {
                target: t,
                has_current: hc,
            } => {
                assert_eq!(t, target);
                assert_eq!(hc, has_current);
                assert_eq!(err.covering_checkpoint(), Some(target_ckpt));
                // The covering checkpoint must surface in the rendered message
                // so on-call debugging needn't recompute checkpoint math.
                assert!(
                    err.to_string().contains(&target_ckpt.to_string()),
                    "error message must include the covering checkpoint value"
                );
            }
            other => panic!("expected CheckpointNotYetPublished, got {other:?}"),
        }
        assert!(
            !err.is_fatal_catchup_failure(),
            "unpublished-checkpoint knit attempt must be transient, not fatal"
        );
        assert!(
            !err.is_hash_mismatch(),
            "unpublished-checkpoint knit attempt must not be a hash mismatch"
        );
    }

    #[test]
    fn test_knit_lcl_mismatch_published_checkpoint_remains_fatal() {
        // When the target checkpoint IS published (HAS currentLedger covers
        // it), the publication gate passes and a genuine knit divergence
        // against the local LCL stays fatal — preserving §11.2 parity for
        // real local corruption (no over-broadening of the transient path).
        let target = 62845437;
        let target_ckpt = crate::checkpoint::checkpoint_containing(target);

        // Gate passes: archive has published the covering checkpoint.
        assert!(
            checkpoint_publication_gate(target_ckpt, target).is_ok(),
            "gate must pass once the covering checkpoint is published"
        );
        // The gate must also pass when the archive frontier is well ahead
        // of the covering checkpoint (steady-state operation).
        assert!(
            checkpoint_publication_gate(target_ckpt + 1000, target).is_ok(),
            "gate must pass when the archive frontier is ahead of the target"
        );
        // And a current-checkpoint divergence is still fatal (case 3).
        let lcl = make_test_lcl(100, h(0x10), h(0x09));
        let entry = make_test_entry(100, h(0xBB), h(0x09));
        let err = knit_to_lcl_decision(&entry, &lcl).unwrap_err();
        assert!(
            matches!(err, HistoryError::KnitLclHashMismatch { .. }),
            "published-checkpoint divergence must stay KnitLclHashMismatch"
        );
        assert!(
            err.is_fatal_catchup_failure(),
            "published-checkpoint knit divergence must remain fatal"
        );
    }
}
