//! Cryptographic verification utilities for history data.
//!
//! This module provides functions to verify the integrity of all data downloaded
//! from history archives. Verification is essential for security: while we download
//! from trusted archives, we cryptographically verify everything to detect corruption
//! or tampering.
//!
//! # Verification Layers
//!
//! ## Header Chain Verification
//!
//! Ledger headers form a cryptographic hash chain where each header contains
//! `previous_ledger_hash` - the SHA-256 hash of the previous header. This
//! ensures that any modification to historical headers would be detected.
//!
//! ## Bucket Hash Verification
//!
//! Each bucket file is identified by its SHA-256 content hash. After downloading,
//! we verify that `SHA256(bucket_content) == expected_hash`. This prevents both
//! accidental corruption and malicious substitution.
//!
//! ## Transaction Set Verification
//!
//! The transaction set hash in the SCP value must match the hash of the
//! downloaded transaction set. For classic sets, this is computed over the
//! concatenation of previous_ledger_hash and transaction XDR. For generalized
//! sets, it is the hash of the entire set XDR.
//!
//! ## Bucket List Hash Verification
//!
//! The final verification: the computed bucket list hash from our reconstructed
//! state must match `header.bucket_list_hash`. This proves that we have correctly
//! rebuilt the entire ledger state.

use crate::{
    archive_state::HistoryArchiveState, checkpoint, HistoryError, Result, GENESIS_LEDGER_SEQ,
};
use henyey_bucket::{BUCKET_LIST_LEVELS, HOT_ARCHIVE_BUCKET_LIST_LEVELS};
use henyey_common::Hash256;
use henyey_crypto::Sha256Hasher;
use henyey_ledger::TransactionSetVariant;
use stellar_xdr::curr::{
    LedgerHeader, LedgerHeaderHistoryEntry, Limits, ScpHistoryEntry, TransactionHistoryResultEntry,
    WriteXdr,
};

/// Optional trust anchors for ledger chain verification (spec §9.2).
///
/// These allow the verification to be anchored to externally trusted hashes,
/// closing the trust gap between "the chain is internally consistent" and
/// "the chain connects to state we independently know to be correct".
#[derive(Debug, Default, Clone)]
pub struct ChainTrustAnchors {
    /// Hash of the ledger header just before the first header in the chain.
    ///
    /// For replay-from-checkpoint scenarios this is the checkpoint header hash.
    /// For online catchup this can be derived from the LCL.
    ///
    /// When set, the first header's `previous_ledger_hash` is verified against
    /// this value.
    pub previous_ledger_hash: Option<Hash256>,

    /// Sequence number of the local LCL (Last Closed Ledger).
    /// Together with `lcl_hash`, enables detection of local state corruption
    /// (spec §9.3 step 2c: if a downloaded header matches LCL seq but has a
    /// different hash, the local state is corrupt).
    pub lcl_seq: Option<u32>,

    /// Hash of the local LCL header. Used together with `lcl_seq`.
    pub lcl_hash: Option<Hash256>,
}

/// Verify that a chain of ledger headers is correctly linked.
///
/// Each header's `previous_ledger_hash` must match the hash of the previous header.
/// Headers must be in ascending order by sequence number.
///
/// # Arguments
///
/// * `headers` - Slice of ledger headers in sequence order (oldest first)
///
/// # Returns
///
/// Ok(()) if the chain is valid, or an error describing the verification failure.
pub fn verify_header_chain(headers: &[LedgerHeader]) -> Result<()> {
    verify_header_chain_inner(headers.iter())
}

/// Like [`verify_header_chain`] but accepts `LedgerHeaderHistoryEntry` slices
/// directly, verifying each entry's advertised hash before checking chain
/// links and avoiding the need to clone every `LedgerHeader` into a contiguous
/// `Vec` just for verification.
pub fn verify_header_chain_from_entries(entries: &[LedgerHeaderHistoryEntry]) -> Result<()> {
    for entry in entries {
        verify_ledger_header_history_entry(entry)?;
    }

    verify_header_chain_inner(entries.iter().map(|e| &e.header))
}

fn verify_header_chain_inner<'a>(
    mut headers: impl Iterator<Item = &'a LedgerHeader>,
) -> Result<()> {
    let Some(first) = headers.next() else {
        return Ok(());
    };

    let mut prev_header = first;
    for curr_header in headers {
        if curr_header.ledger_seq != prev_header.ledger_seq + 1 {
            return Err(HistoryError::InvalidSequence {
                expected: prev_header.ledger_seq + 1,
                got: curr_header.ledger_seq,
            });
        }

        let prev_hash = compute_header_hash(prev_header)?;
        let expected_prev_hash = Hash256::from(curr_header.previous_ledger_hash.clone());
        if prev_hash != expected_prev_hash {
            return Err(HistoryError::InvalidPreviousHash {
                ledger: curr_header.ledger_seq,
            });
        }
        prev_header = curr_header;
    }

    Ok(())
}

/// Verify trust anchors against a verified header chain (spec §9.2–§9.5).
///
/// This function should be called **after** [`verify_header_chain`] has confirmed
/// the internal consistency of the header slice.  It checks that the chain
/// connects to externally trusted state:
///
/// 1. **Bottom anchor**: the first header's `previous_ledger_hash` matches the
///    provided `previous_ledger_hash` (e.g., the checkpoint header hash).
/// 2. **LCL comparison**: if any header in the chain corresponds to the local
///    LCL sequence, its hash must match `lcl_hash`.  A mismatch means the
///    local state diverges from the archive-provided (SCP-verified) chain,
///    indicating local state corruption.
///
/// # Errors
///
/// Returns [`HistoryError::VerificationHashMismatch`] with
/// [`VerifyHashKind::BottomAnchor`](crate::error::VerifyHashKind::BottomAnchor)
/// if the bottom anchor check fails, or with
/// [`VerifyHashKind::Lcl`](crate::error::VerifyHashKind::Lcl) if the LCL hash
/// check fails. May also return [`HistoryError::CorruptHeader`] if
/// `compute_header_hash` fails to encode a header (only on the LCL-comparison
/// path).
pub fn verify_chain_anchors(headers: &[LedgerHeader], anchors: &ChainTrustAnchors) -> Result<()> {
    if headers.is_empty() {
        return Ok(());
    }

    // 1. Bottom anchor: first header must link to the provided previous hash.
    if let Some(expected_prev) = &anchors.previous_ledger_hash {
        let first = &headers[0];
        let actual_prev = Hash256::from(first.previous_ledger_hash.clone());
        if actual_prev != *expected_prev {
            return Err(crate::error::VerifyHashMismatchInfo::log_and_new(
                crate::error::VerifyHashKind::BottomAnchor,
                Some(first.ledger_seq),
                *expected_prev,
                actual_prev,
            )
            .into());
        }
    }

    // 2. LCL comparison: if a header matches the LCL sequence, its hash must
    //    match.  This detects local state corruption (spec §9.3 step 2c).
    if let (Some(lcl_seq), Some(lcl_hash)) = (anchors.lcl_seq, &anchors.lcl_hash) {
        for header in headers {
            if header.ledger_seq == lcl_seq {
                let header_hash = compute_header_hash(header)?;
                if header_hash != *lcl_hash {
                    return Err(crate::error::VerifyHashMismatchInfo::log_and_new(
                        crate::error::VerifyHashKind::Lcl,
                        Some(lcl_seq),
                        *lcl_hash,
                        header_hash,
                    )
                    .into());
                }
                break;
            }
        }
    }

    Ok(())
}

// ─── Reverse-Walk Verification (§9.2–§9.5) ─────────────────────────────────

/// Trust source for the highest ledger in the verification range (§9.2).
///
/// Determines whether local-state disagreement is fatal (non-retriable) or
/// merely indicates a potentially corrupt archive (retriable).
#[derive(Debug, Clone)]
pub enum TrustSource {
    /// Hash from SCP consensus — enables fatal failure detection (§9.5).
    /// If the verified chain disagrees with local state, the node must halt.
    Scp { seq: u32, hash: Hash256 },
    /// No trusted top hash. Verification checks internal consistency and
    /// LCL agreement, but LCL disagreement is not flagged as fatal.
    None,
}

/// Configuration for reverse-walk verification.
///
/// This is catchup-specific and should NOT be added to the generic `ReplayConfig`.
#[derive(Debug, Clone)]
pub struct ReverseWalkConfig {
    /// Trust source for the highest ledger.
    pub trust_source: TrustSource,
    /// LCL (seq, hash) for §9.3 step 2d comparison.
    /// Obtain from `LedgerManager::header_snapshot()` for atomicity.
    pub lcl: Option<(u32, Hash256)>,
    /// Maximum supported protocol version (§9.3 step 2e).
    pub max_supported_version: u32,
    /// Minimum supported protocol version.
    pub min_supported_version: u32,
}

/// Result of reverse-walk verification (returned on structural success).
#[derive(Debug, Clone)]
pub struct ReverseWalkResult {
    /// Fatal failure (§9.5): local state disagrees with an SCP-anchored chain.
    /// The node MUST NOT retry catchup.
    pub fatal_failure: bool,
}

/// Verify a chain of ledger headers using the reverse-walk algorithm (§9.2–§9.5).
///
/// Headers must be in ascending order by sequence number. Internally, this
/// function partitions them into checkpoints and processes from highest to
/// lowest, threading trust from the top-level anchor.
///
/// **Prerequisite**: Each header's content hash was verified against the
/// archive's advertised hash during download (per-checkpoint
/// `verify_header_chain_from_entries`). This function verifies chain links
/// and trust anchors, NOT individual header integrity.
///
/// # Returns
/// - `Ok(result)` — chain verified; check `result.fatal_failure`
/// - `Err(InvalidPreviousHash)` — broken hash link (retriable)
/// - `Err(InvalidSequence)` — sequence gap (retriable)
/// - `Err(VerificationHashMismatch(TopAnchor))` — trust anchor mismatch (retriable)
/// - `Err(UnsupportedLedgerVersion)` — version outside supported range (fatal)
/// - `Err(FatalChainDisagreement)` — LCL disagrees + SCP trust (fatal)
pub fn verify_reverse_walk(
    headers: &[LedgerHeader],
    config: &ReverseWalkConfig,
) -> Result<ReverseWalkResult> {
    if headers.is_empty() {
        return Ok(ReverseWalkResult {
            fatal_failure: false,
        });
    }

    // Check protocol versions for all headers first (§9.3 step 2e).
    // Version 0 is allowed: it's the genesis protocol state before any upgrade,
    // always present in CATCHUP_COMPLETE histories. Mirrors begin_close's gate.
    for header in headers {
        if header.ledger_version != 0
            && (header.ledger_version > config.max_supported_version
                || header.ledger_version < config.min_supported_version)
        {
            return Err(HistoryError::UnsupportedLedgerVersion {
                ledger: header.ledger_seq,
                version: header.ledger_version,
                min: config.min_supported_version,
                max: config.max_supported_version,
            });
        }
    }

    // Partition headers into checkpoint groups.
    // Each group contains headers belonging to the same checkpoint, in ascending order.
    let mut checkpoint_groups: Vec<&[LedgerHeader]> = Vec::new();
    let mut group_start = 0;
    for i in 1..headers.len() {
        let prev_cp = checkpoint::checkpoint_containing(headers[i - 1].ledger_seq);
        let curr_cp = checkpoint::checkpoint_containing(headers[i].ledger_seq);
        if curr_cp != prev_cp {
            checkpoint_groups.push(&headers[group_start..i]);
            group_start = i;
        }
    }
    checkpoint_groups.push(&headers[group_start..]);

    // Process from highest checkpoint to lowest (reverse order).
    checkpoint_groups.reverse();

    // Cross-checkpoint link: hash of the last header in the next-higher group,
    // used to verify continuity between adjacent checkpoint groups.
    let mut cross_checkpoint_link: Option<(u32, Hash256)> = None;

    // Trust anchor from SCP consensus — verified against the header at the
    // specified seq, which may be in any group.
    let mut pending_trust_anchor: Option<(u32, Hash256)> = match &config.trust_source {
        TrustSource::Scp { seq, hash } => Some((*seq, *hash)),
        TrustSource::None => None,
    };

    let mut local_state_disagrees = false;

    for group in &checkpoint_groups {
        // Verify internal chain links within this checkpoint (forward).
        for i in 1..group.len() {
            let prev = &group[i - 1];
            let curr = &group[i];

            // Sequence continuity.
            if curr.ledger_seq != prev.ledger_seq + 1 {
                return Err(HistoryError::InvalidSequence {
                    expected: prev.ledger_seq + 1,
                    got: curr.ledger_seq,
                });
            }

            // Hash chain link.
            let prev_hash = compute_header_hash(prev)?;
            let expected_prev_hash = Hash256::from(curr.previous_ledger_hash.clone());
            if prev_hash != expected_prev_hash {
                return Err(HistoryError::InvalidPreviousHash {
                    ledger: curr.ledger_seq,
                });
            }
        }

        // Verify cross-checkpoint link: the higher group's outgoing link
        // must match this group's last header hash.
        if let Some((expected_seq, expected_hash)) = &cross_checkpoint_link {
            let last = group.last().unwrap();
            debug_assert_eq!(
                last.ledger_seq, *expected_seq,
                "cross-checkpoint link seq mismatch — groups must be contiguous"
            );
            let last_hash = compute_header_hash(last)?;
            if last_hash != *expected_hash {
                return Err(crate::error::VerifyHashMismatchInfo::log_and_new(
                    crate::error::VerifyHashKind::TopAnchor,
                    Some(last.ledger_seq),
                    *expected_hash,
                    last_hash,
                )
                .into());
            }
        }

        // Verify trust anchor if it falls within this group.
        if let Some((anchor_seq, anchor_hash)) = &pending_trust_anchor {
            let last = group.last().unwrap();
            let first = &group[0];
            if *anchor_seq > last.ledger_seq {
                // Trust anchor points beyond our highest group — error.
                // (Only reachable on the first group iteration.)
                return Err(HistoryError::InvalidSequence {
                    expected: *anchor_seq,
                    got: last.ledger_seq,
                });
            } else if *anchor_seq >= first.ledger_seq {
                // Trust anchor is in this group — find and verify.
                let target = group
                    .iter()
                    .find(|h| h.ledger_seq == *anchor_seq)
                    .expect("anchor seq in group range but not found");
                let target_hash = compute_header_hash(target)?;
                if target_hash != *anchor_hash {
                    return Err(crate::error::VerifyHashMismatchInfo::log_and_new(
                        crate::error::VerifyHashKind::TopAnchor,
                        Some(target.ledger_seq),
                        *anchor_hash,
                        target_hash,
                    )
                    .into());
                }
                pending_trust_anchor = None; // consumed
            }
            // else: anchor is in a lower group, keep pending
        }

        // Record outgoing link for the next (lower) group.
        let first = &group[0];
        cross_checkpoint_link = Some((
            first.ledger_seq - 1,
            Hash256::from(first.previous_ledger_hash.clone()),
        ));

        // LCL-overlap (§9.3 step 2c) and LCL+1 (§9.3 step 2d) comparisons.
        // Mirrors stellar-core VerifyLedgerChainWork.cpp:222-235: step 2c (a
        // downloaded header sitting exactly at lcl_seq must hash-match the local
        // LCL) and step 2d (the header at lcl_seq + 1 must link back to the LCL
        // hash) are mutually exclusive per header (distinct seqs), so they form
        // an if / else if. On disagreement we set the flag and continue — the
        // §9.5 block below converts it to fatal (SCP trust) or retriable (None).
        if let Some((lcl_seq, lcl_hash)) = &config.lcl {
            for header in group.iter() {
                if header.ledger_seq == *lcl_seq {
                    // Step 2c: header at lcl_seq — its content hash must match.
                    let header_hash = compute_header_hash(header)?;
                    if header_hash != *lcl_hash {
                        local_state_disagrees = true;
                    }
                } else if header.ledger_seq == lcl_seq + 1 {
                    // Step 2d: header at lcl_seq + 1 — its previous_ledger_hash
                    // must link back to the local LCL hash.
                    let prev_hash = Hash256::from(header.previous_ledger_hash.clone());
                    if prev_hash != *lcl_hash {
                        local_state_disagrees = true;
                    }
                }
            }
        }
    }

    // If the trust anchor was never consumed, it refers to a seq below our
    // lowest header — we cannot verify it.
    if let Some((anchor_seq, _)) = pending_trust_anchor {
        let lowest_seq = checkpoint_groups
            .last()
            .and_then(|g| g.first())
            .map(|h| h.ledger_seq)
            .unwrap_or(0);
        return Err(HistoryError::InvalidSequence {
            expected: anchor_seq,
            got: lowest_seq,
        });
    }

    // §9.5: Fatal failure determination.
    if local_state_disagrees {
        match &config.trust_source {
            TrustSource::Scp { .. } => {
                return Err(HistoryError::FatalChainDisagreement);
            }
            TrustSource::None => {
                // Without SCP trust, LCL disagreement is treated as a broken
                // chain link (possibly corrupt archive data).
                return Err(HistoryError::InvalidPreviousHash {
                    ledger: config.lcl.map(|(s, _)| s + 1).unwrap_or(0),
                });
            }
        }
    }

    Ok(ReverseWalkResult {
        fatal_failure: false,
    })
}
///
/// # Arguments
///
/// * `data` - Raw bucket data (typically decompressed XDR)
/// * `expected_hash` - The expected SHA-256 hash of the bucket
///
/// # Returns
///
/// Ok(()) if the hash matches, or an error if it doesn't.
pub fn verify_bucket_hash(data: &[u8], expected_hash: &Hash256) -> Result<()> {
    let actual_hash = Hash256::hash(data);

    if actual_hash != *expected_hash {
        return Err(crate::error::VerifyHashMismatchInfo::log_and_new(
            crate::error::VerifyHashKind::Bucket,
            None,
            *expected_hash,
            actual_hash,
        )
        .into());
    }

    Ok(())
}

/// Verify that a ledger header's bucket list hash matches the expected value.
///
/// This verifies that the ledger state (represented by the BucketList) matches
/// what the header claims.
///
/// # Arguments
///
/// * `header` - The ledger header to verify
/// * `bucket_list_hash` - The computed hash of the BucketList
///
/// # Returns
///
/// Ok(()) if the hashes match, or an error if they don't.
pub fn verify_ledger_hash(header: &LedgerHeader, bucket_list_hash: &Hash256) -> Result<()> {
    let header_bucket_hash = Hash256::from(header.bucket_list_hash.clone());

    if header_bucket_hash != *bucket_list_hash {
        return Err(crate::error::VerifyHashMismatchInfo::log_and_new(
            crate::error::VerifyHashKind::BucketList,
            Some(header.ledger_seq),
            header_bucket_hash,
            *bucket_list_hash,
        )
        .into());
    }

    Ok(())
}

/// Compute the SHA-256 hash of a ledger header.
///
/// This is the hash that gets stored in the next ledger's `previous_ledger_hash`.
pub fn compute_header_hash(header: &LedgerHeader) -> Result<Hash256> {
    let xdr_bytes = header
        .to_xdr(stellar_xdr::curr::Limits::none())
        .map_err(|e| HistoryError::CorruptHeader {
            ledger: header.ledger_seq,
            detail: format!("failed to encode header: {}", e),
        })?;

    Ok(Hash256::hash(&xdr_bytes))
}

/// Verify a ledger-header history entry's advertised hash.
///
/// Stellar-core rejects `LedgerHeaderHistoryEntry` records whose `hash` field
/// does not match `SHA256(XDR(header))` before trusting that hash or using the
/// entry in ledger-chain verification.
pub fn verify_ledger_header_history_entry(entry: &LedgerHeaderHistoryEntry) -> Result<Hash256> {
    let computed_hash = compute_header_hash(&entry.header)?;
    let advertised_hash = Hash256::from(entry.hash.clone());

    if computed_hash != advertised_hash {
        return Err(crate::error::VerifyHashMismatchInfo::log_and_new(
            crate::error::VerifyHashKind::LedgerHeaderEntry,
            Some(entry.header.ledger_seq),
            advertised_hash,
            computed_hash,
        )
        .into());
    }

    Ok(computed_hash)
}

/// Verify a transaction result set against the ledger header.
///
/// The hash of the transaction result set must match what's in the header.
/// For the genesis ledger (seq == 1), an empty result set is accepted without
/// hash verification, matching stellar-core's `VerifyTxResultsWork`.
///
/// # Arguments
///
/// * `header` - The ledger header
/// * `tx_result_set_xdr` - XDR-encoded transaction result set
pub fn verify_tx_result_set(header: &LedgerHeader, tx_result_set_xdr: &[u8]) -> Result<()> {
    // Genesis ledger exception: stellar-core skips verification when the result
    // set is empty at the genesis ledger because it has no transactions.
    if header.ledger_seq == GENESIS_LEDGER_SEQ && tx_result_set_xdr.is_empty() {
        return Ok(());
    }

    let actual_hash = Hash256::hash(tx_result_set_xdr);
    let expected_hash = Hash256::from(header.tx_set_result_hash.clone());

    if actual_hash != expected_hash {
        return Err(crate::error::VerifyHashMismatchInfo::log_and_new(
            crate::error::VerifyHashKind::TxResultSet,
            Some(header.ledger_seq),
            expected_hash,
            actual_hash,
        )
        .into());
    }

    Ok(())
}

/// Compute the transaction set hash according to protocol rules.
pub fn compute_tx_set_hash(tx_set: &TransactionSetVariant) -> Result<Hash256> {
    match tx_set {
        TransactionSetVariant::Classic(set) => {
            let mut hasher = Sha256Hasher::new();
            hasher.update(&set.previous_ledger_hash.0);
            for tx in set.txs.iter() {
                let bytes = tx.to_xdr(Limits::none()).map_err(|e| {
                    HistoryError::CatchupFailed(format!("failed to encode tx: {}", e))
                })?;
                hasher.update(&bytes);
            }
            Ok(hasher.finalize())
        }
        TransactionSetVariant::Generalized(set) => {
            let bytes = set.to_xdr(Limits::none()).map_err(|e| {
                HistoryError::CatchupFailed(format!("failed to encode tx set: {}", e))
            })?;
            Ok(Hash256::hash(&bytes))
        }
    }
}

/// Verify a transaction set against the ledger header.
///
/// The hash of the transaction set must match what's in the header.
///
/// # Arguments
///
/// * `header` - The ledger header
/// * `tx_set` - Transaction set variant
pub fn verify_tx_set(header: &LedgerHeader, tx_set: &TransactionSetVariant) -> Result<()> {
    let actual_hash = compute_tx_set_hash(tx_set)?;
    let expected_hash = Hash256::from(header.scp_value.tx_set_hash.clone());

    if actual_hash != expected_hash {
        let header_prev_hash = Hash256::from(header.previous_ledger_hash.clone());
        let tx_set_prev_hash = tx_set.previous_ledger_hash();
        let tx_set_format = match tx_set {
            TransactionSetVariant::Classic(_) => "classic",
            TransactionSetVariant::Generalized(_) => "generalized_v1",
        };

        tracing::error!(
            ledger_seq = header.ledger_seq,
            header_ledger_version = header.ledger_version,
            %header_prev_hash,
            %tx_set_prev_hash,
            %actual_hash,
            %expected_hash,
            tx_set_format,
            "verify_tx_set hash mismatch"
        );

        return Err(crate::error::TxSetHashMismatchInfo::new(
            expected_hash,
            actual_hash,
            header.ledger_version,
            header_prev_hash,
            tx_set_prev_hash,
            tx_set_format,
        )
        .into_error(header.ledger_seq));
    }

    Ok(())
}

/// Verify that a header links correctly to a known trusted header.
///
/// This is used to verify headers downloaded from history against
/// a header we received via SCP (which we trust).
///
/// # Arguments
///
/// * `downloaded_header` - Header downloaded from history archive
/// * `trusted_header` - Header received via SCP consensus
pub fn verify_header_matches_trusted(
    downloaded_header: &LedgerHeader,
    trusted_header: &LedgerHeader,
) -> Result<()> {
    if downloaded_header.ledger_seq != trusted_header.ledger_seq {
        return Err(HistoryError::InvalidSequence {
            expected: trusted_header.ledger_seq,
            got: downloaded_header.ledger_seq,
        });
    }

    let downloaded_hash = compute_header_hash(downloaded_header)?;
    let trusted_hash = compute_header_hash(trusted_header)?;

    if downloaded_hash != trusted_hash {
        return Err(crate::error::VerifyHashMismatchInfo::log_and_new(
            crate::error::VerifyHashKind::TrustedHeader,
            Some(downloaded_header.ledger_seq),
            trusted_hash,
            downloaded_hash,
        )
        .into());
    }

    Ok(())
}

/// Result of verifying a complete catchup dataset.
#[derive(Debug, Clone)]
pub struct VerificationResult {
    /// Number of headers verified.
    pub headers_verified: u32,
    /// Number of buckets verified.
    pub buckets_verified: u32,
    /// Number of ledgers verified.
    pub ledgers_verified: u32,
    /// The final ledger hash.
    pub final_ledger_hash: Hash256,
}

/// Verify the HAS network passphrase matches the expected passphrase.
///
/// Per stellar-core semantics: if the HAS has a non-empty `networkPassphrase`
/// field and it does not match the configured passphrase, catchup fails.
/// If the HAS passphrase is absent or empty (version 1 archives), validation
/// is skipped.
///
/// # Arguments
///
/// * `has` - The History Archive State to validate
/// * `expected` - The configured network passphrase to compare against
pub fn verify_has_passphrase(has: &HistoryArchiveState, expected: &str) -> Result<()> {
    if let Some(ref has_passphrase) = has.network_passphrase {
        if !has_passphrase.is_empty() && has_passphrase != expected {
            return Err(HistoryError::VerificationFailed(format!(
                "HAS network passphrase mismatch: expected '{}', got '{}'",
                expected, has_passphrase
            )));
        }
    }
    Ok(())
}

/// Verify the History Archive State (HAS) structure.
///
/// The HAS contains bucket hashes for each level of the BucketList.
/// This function verifies the structure is well-formed.
pub fn verify_has_structure(has: &HistoryArchiveState) -> Result<()> {
    // Spec: CATCHUP_SPEC §3.1 — the number of levels MUST equal BUCKET_LIST_LEVELS.
    if has.current_buckets.len() != BUCKET_LIST_LEVELS {
        return Err(HistoryError::VerificationFailed(format!(
            "HAS has {} bucket levels, expected {}",
            has.current_buckets.len(),
            BUCKET_LIST_LEVELS
        )));
    }

    // Check version is supported
    if has.version < 1 || has.version > 2 {
        return Err(HistoryError::VerificationFailed(format!(
            "unsupported HAS version: {}",
            has.version
        )));
    }

    // Spec: CATCHUP_SPEC §3.1 — version 2 MUST include a networkPassphrase field.
    if has.version >= 2 && has.network_passphrase.is_none() {
        return Err(HistoryError::VerificationFailed(
            "HAS version 2 requires networkPassphrase field".to_string(),
        ));
    }

    // Spec: CATCHUP_SPEC §4.4 — version 2 introduced hotArchiveBuckets.
    // When present, it must have exactly HOT_ARCHIVE_BUCKET_LIST_LEVELS entries.
    // Version 1 must NOT have hotArchiveBuckets.
    // Version 2 MUST have hotArchiveBuckets (stellar-core deserializes it
    // unconditionally for v2; see HistoryArchive.h:157-160,211-214).
    match (has.version, &has.hot_archive_buckets) {
        (1, Some(_)) => {
            return Err(HistoryError::VerificationFailed(
                "HAS version 1 must not contain hotArchiveBuckets field".to_string(),
            ));
        }
        (2, None) => {
            return Err(HistoryError::VerificationFailed(
                "HAS version 2 requires hotArchiveBuckets field".to_string(),
            ));
        }
        (2, Some(hot_buckets)) if hot_buckets.len() != HOT_ARCHIVE_BUCKET_LIST_LEVELS => {
            return Err(HistoryError::VerificationFailed(format!(
                "hotArchiveBuckets has {} levels, expected {}",
                hot_buckets.len(),
                HOT_ARCHIVE_BUCKET_LIST_LEVELS
            )));
        }
        _ => {}
    }

    Ok(())
}

/// Verify that the HAS is for the expected checkpoint.
pub fn verify_has_checkpoint(has: &HistoryArchiveState, expected: u32) -> Result<()> {
    if has.current_ledger != expected {
        return Err(HistoryError::VerificationFailed(format!(
            "HAS checkpoint mismatch: expected {}, got {}",
            expected, has.current_ledger
        )));
    }
    Ok(())
}

/// Verify SCP history entries contain quorum sets for all referenced envelopes.
pub fn verify_scp_history_entries(entries: &[ScpHistoryEntry]) -> Result<()> {
    for entry in entries {
        let ScpHistoryEntry::V0(v0) = entry;
        let mut qset_hashes = std::collections::HashSet::new();
        for qset in v0.quorum_sets.iter() {
            qset_hashes.insert(Hash256::hash_xdr(qset));
        }

        for envelope in v0.ledger_messages.messages.iter() {
            let hash = henyey_common::scp_quorum_set_hash(&envelope.statement);
            let hash256 = Hash256::from_bytes(hash.0);
            if !qset_hashes.contains(&hash256) {
                return Err(HistoryError::VerificationFailed(format!(
                    "missing quorum set {} in scp history",
                    hash256.to_hex()
                )));
            }
        }
    }

    Ok(())
}

/// Verify that transaction result entries within a checkpoint are correctly ordered.
///
/// Validates that:
/// 1. All ledger sequence numbers fall within the checkpoint range
/// 2. Sequence numbers are strictly increasing
///
/// # Arguments
///
/// * `entries` - Transaction result entries to verify
/// * `checkpoint` - The checkpoint ledger sequence number
pub fn verify_tx_result_ordering(
    entries: &[TransactionHistoryResultEntry],
    checkpoint: u32,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let (range_start, range_end) = checkpoint::checkpoint_range(checkpoint);

    let mut prev_seq = None;
    for entry in entries {
        // Check within checkpoint range
        if entry.ledger_seq < range_start || entry.ledger_seq > range_end {
            return Err(HistoryError::VerificationFailed(format!(
                "tx result entry ledger {} outside checkpoint range [{}, {}]",
                entry.ledger_seq, range_start, range_end
            )));
        }

        // Check strictly increasing
        if let Some(prev) = prev_seq {
            if entry.ledger_seq <= prev {
                return Err(HistoryError::VerificationFailed(format!(
                    "tx result entries not strictly increasing: {} followed by {}",
                    prev, entry.ledger_seq
                )));
            }
        }

        prev_seq = Some(entry.ledger_seq);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{Hash, LedgerHeaderHistoryEntryExt, StellarValue, TimePoint, VecM};

    fn make_test_header(seq: u32, prev_hash: Hash256) -> LedgerHeader {
        LedgerHeader {
            ledger_version: 20,
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
        }
    }

    #[test]
    fn test_verify_header_chain_valid() {
        let header1 = make_test_header(1, Hash256::ZERO);
        let hash1 = compute_header_hash(&header1).unwrap();
        let header2 = make_test_header(2, hash1);
        let hash2 = compute_header_hash(&header2).unwrap();
        let header3 = make_test_header(3, hash2);

        let headers = vec![header1, header2, header3];
        assert!(verify_header_chain(&headers).is_ok());
    }

    #[test]
    fn test_verify_header_chain_broken() {
        let header1 = make_test_header(1, Hash256::ZERO);
        let header2 = make_test_header(2, Hash256::ZERO); // Wrong prev hash
        let headers = vec![header1, header2];

        assert!(verify_header_chain(&headers).is_err());
    }

    #[test]
    fn test_verify_header_chain_non_consecutive() {
        let header1 = make_test_header(1, Hash256::ZERO);
        let hash1 = compute_header_hash(&header1).unwrap();
        let header3 = make_test_header(3, hash1); // Skipped sequence 2

        let headers = vec![header1, header3];
        assert!(verify_header_chain(&headers).is_err());
    }

    fn make_header_history_entry(header: LedgerHeader, hash: Hash256) -> LedgerHeaderHistoryEntry {
        LedgerHeaderHistoryEntry {
            hash: hash.into(),
            header,
            ext: LedgerHeaderHistoryEntryExt::default(),
        }
    }

    #[test]
    fn test_verify_ledger_header_history_entry_validates_advertised_hash() {
        let header = make_test_header(1, Hash256::ZERO);
        let expected_hash = compute_header_hash(&header).unwrap();
        let entry = make_header_history_entry(header, expected_hash);

        assert_eq!(
            verify_ledger_header_history_entry(&entry).unwrap(),
            expected_hash
        );
    }

    #[test]
    fn test_verify_ledger_header_history_entry_rejects_mismatched_hash() {
        let header = make_test_header(1, Hash256::ZERO);
        let entry = make_header_history_entry(header, Hash256::hash(b"wrong"));

        assert!(verify_ledger_header_history_entry(&entry).is_err());
    }

    #[test]
    fn test_verify_header_chain_from_entries_rejects_first_entry_hash_mismatch() {
        let header = make_test_header(1, Hash256::ZERO);
        let entry = make_header_history_entry(header, Hash256::hash(b"wrong"));

        assert!(verify_header_chain_from_entries(&[entry]).is_err());
    }

    #[test]
    fn test_verify_header_chain_from_entries_rejects_later_entry_hash_mismatch() {
        let header1 = make_test_header(1, Hash256::ZERO);
        let hash1 = compute_header_hash(&header1).unwrap();
        let header2 = make_test_header(2, hash1);
        let entry1 = make_header_history_entry(header1, hash1);
        let entry2 = make_header_history_entry(header2, Hash256::hash(b"wrong"));

        assert!(verify_header_chain_from_entries(&[entry1, entry2]).is_err());
    }

    #[test]
    fn test_verify_bucket_hash() {
        let data = b"test bucket data";
        let correct_hash = Hash256::hash(data);
        let wrong_hash = Hash256::hash(b"different data");

        assert!(verify_bucket_hash(data, &correct_hash).is_ok());
        assert!(verify_bucket_hash(data, &wrong_hash).is_err());
    }

    #[test]
    fn test_verify_header_chain_empty() {
        let headers: Vec<LedgerHeader> = vec![];
        assert!(verify_header_chain(&headers).is_ok());
    }

    #[test]
    fn test_verify_header_chain_single() {
        let header = make_test_header(1, Hash256::ZERO);
        let headers = vec![header];
        assert!(verify_header_chain(&headers).is_ok());
    }

    // Item 1: Genesis ledger exception tests
    #[test]
    fn test_verify_tx_result_set_genesis_empty() {
        let header = make_test_header(GENESIS_LEDGER_SEQ, Hash256::ZERO);
        // Empty result set at genesis should pass without hash check
        assert!(verify_tx_result_set(&header, &[]).is_ok());
    }

    #[test]
    fn test_verify_tx_result_set_genesis_nonempty() {
        let header = make_test_header(GENESIS_LEDGER_SEQ, Hash256::ZERO);
        // Non-empty result set at genesis should still verify hash
        let result = verify_tx_result_set(&header, &[1, 2, 3]);
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_tx_result_set_non_genesis_empty() {
        let header = make_test_header(2, Hash256::ZERO);
        // Empty result at non-genesis should still verify hash
        let result = verify_tx_result_set(&header, &[]);
        // This will fail unless the hash happens to match (it won't for zero header)
        // The hash of empty bytes won't match the zero tx_set_result_hash
        let empty_hash = Hash256::empty_hash();
        let expected_hash = Hash256::from(header.tx_set_result_hash.clone());
        if *empty_hash != expected_hash {
            assert!(result.is_err());
        }
    }

    // Item 4: Transaction result ordering tests
    #[test]
    fn test_verify_tx_result_ordering_valid() {
        use stellar_xdr::curr::{
            TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionResultSet,
            VecM,
        };

        let entries = vec![
            TransactionHistoryResultEntry {
                ledger_seq: 64,
                tx_result_set: TransactionResultSet {
                    results: VecM::default(),
                },
                ext: TransactionHistoryResultEntryExt::V0,
            },
            TransactionHistoryResultEntry {
                ledger_seq: 65,
                tx_result_set: TransactionResultSet {
                    results: VecM::default(),
                },
                ext: TransactionHistoryResultEntryExt::V0,
            },
            TransactionHistoryResultEntry {
                ledger_seq: 127,
                tx_result_set: TransactionResultSet {
                    results: VecM::default(),
                },
                ext: TransactionHistoryResultEntryExt::V0,
            },
        ];
        assert!(verify_tx_result_ordering(&entries, 127).is_ok());
    }

    #[test]
    fn test_verify_tx_result_ordering_out_of_range() {
        use stellar_xdr::curr::{
            TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionResultSet,
            VecM,
        };

        let entries = vec![TransactionHistoryResultEntry {
            ledger_seq: 200, // Outside checkpoint 127 (range 64-127)
            tx_result_set: TransactionResultSet {
                results: VecM::default(),
            },
            ext: TransactionHistoryResultEntryExt::V0,
        }];
        assert!(verify_tx_result_ordering(&entries, 127).is_err());
    }

    #[test]
    fn test_verify_tx_result_ordering_not_increasing() {
        use stellar_xdr::curr::{
            TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionResultSet,
            VecM,
        };

        let entries = vec![
            TransactionHistoryResultEntry {
                ledger_seq: 65,
                tx_result_set: TransactionResultSet {
                    results: VecM::default(),
                },
                ext: TransactionHistoryResultEntryExt::V0,
            },
            TransactionHistoryResultEntry {
                ledger_seq: 65, // Duplicate, not strictly increasing
                tx_result_set: TransactionResultSet {
                    results: VecM::default(),
                },
                ext: TransactionHistoryResultEntryExt::V0,
            },
        ];
        assert!(verify_tx_result_ordering(&entries, 127).is_err());
    }

    #[test]
    fn test_verify_tx_result_ordering_empty() {
        let entries: Vec<TransactionHistoryResultEntry> = vec![];
        assert!(verify_tx_result_ordering(&entries, 127).is_ok());
    }

    #[test]
    fn test_verify_tx_result_ordering_descending() {
        use stellar_xdr::curr::{
            TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionResultSet,
            VecM,
        };

        let entries = vec![
            TransactionHistoryResultEntry {
                ledger_seq: 100,
                tx_result_set: TransactionResultSet {
                    results: VecM::default(),
                },
                ext: TransactionHistoryResultEntryExt::V0,
            },
            TransactionHistoryResultEntry {
                ledger_seq: 80, // Descending, not strictly increasing
                tx_result_set: TransactionResultSet {
                    results: VecM::default(),
                },
                ext: TransactionHistoryResultEntryExt::V0,
            },
        ];
        assert!(verify_tx_result_ordering(&entries, 127).is_err());
    }

    #[test]
    fn test_verify_tx_result_ordering_single_entry() {
        use stellar_xdr::curr::{
            TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionResultSet,
            VecM,
        };

        let entries = vec![TransactionHistoryResultEntry {
            ledger_seq: 100,
            tx_result_set: TransactionResultSet {
                results: VecM::default(),
            },
            ext: TransactionHistoryResultEntryExt::V0,
        }];
        assert!(verify_tx_result_ordering(&entries, 127).is_ok());
    }

    // ── verify_chain_anchors tests ──────────────────────────────────────

    #[test]
    fn test_chain_anchors_empty_headers_is_noop() {
        let anchors = ChainTrustAnchors {
            previous_ledger_hash: Some(Hash256::ZERO),
            lcl_seq: Some(5),
            lcl_hash: Some(Hash256::ZERO),
        };
        assert!(verify_chain_anchors(&[], &anchors).is_ok());
    }

    #[test]
    fn test_chain_anchors_no_anchors_is_noop() {
        let h1 = make_test_header(1, Hash256::ZERO);
        let hash1 = compute_header_hash(&h1).unwrap();
        let h2 = make_test_header(2, hash1);
        assert!(verify_chain_anchors(&[h1, h2], &ChainTrustAnchors::default()).is_ok());
    }

    #[test]
    fn test_chain_anchors_valid_bottom_anchor() {
        let prev_hash = Hash256::hash(b"checkpoint header bytes");
        let h1 = make_test_header(10, prev_hash);
        let anchors = ChainTrustAnchors {
            previous_ledger_hash: Some(prev_hash),
            ..Default::default()
        };
        assert!(verify_chain_anchors(&[h1], &anchors).is_ok());
    }

    #[test]
    fn test_chain_anchors_invalid_bottom_anchor() {
        let h1 = make_test_header(10, Hash256::ZERO);
        let wrong_hash = Hash256::hash(b"wrong");
        let anchors = ChainTrustAnchors {
            previous_ledger_hash: Some(wrong_hash),
            ..Default::default()
        };
        let result = verify_chain_anchors(&[h1], &anchors);
        let err = result.unwrap_err();
        match &err {
            HistoryError::VerificationHashMismatch(info) => {
                assert_eq!(info.kind(), crate::error::VerifyHashKind::BottomAnchor);
                assert_eq!(info.ledger(), Some(10));
                assert_eq!(info.expected(), wrong_hash);
                assert_eq!(info.actual(), Hash256::ZERO);
            }
            other => panic!("expected VerificationHashMismatch, got: {other:?}"),
        }
    }

    #[test]
    fn test_chain_anchors_lcl_hash_match() {
        let h1 = make_test_header(5, Hash256::ZERO);
        let h1_hash = compute_header_hash(&h1).unwrap();
        let anchors = ChainTrustAnchors {
            previous_ledger_hash: None,
            lcl_seq: Some(5),
            lcl_hash: Some(h1_hash),
        };
        assert!(verify_chain_anchors(&[h1], &anchors).is_ok());
    }

    #[test]
    fn test_chain_anchors_lcl_hash_mismatch_detects_corruption() {
        let h1 = make_test_header(5, Hash256::ZERO);
        let h1_hash = compute_header_hash(&h1).unwrap();
        let wrong_lcl_hash = Hash256::hash(b"corrupted local state");
        let anchors = ChainTrustAnchors {
            previous_ledger_hash: None,
            lcl_seq: Some(5),
            lcl_hash: Some(wrong_lcl_hash),
        };
        let result = verify_chain_anchors(&[h1], &anchors);
        let err = result.unwrap_err();
        match &err {
            HistoryError::VerificationHashMismatch(info) => {
                assert_eq!(info.kind(), crate::error::VerifyHashKind::Lcl);
                assert_eq!(info.ledger(), Some(5));
                assert_eq!(info.expected(), wrong_lcl_hash);
                assert_eq!(info.actual(), h1_hash);
            }
            other => panic!("expected VerificationHashMismatch, got: {other:?}"),
        }
    }

    #[test]
    fn test_chain_anchors_lcl_seq_not_in_chain_is_ok() {
        // LCL seq doesn't appear in the chain — no comparison to make
        let h1 = make_test_header(10, Hash256::ZERO);
        let anchors = ChainTrustAnchors {
            previous_ledger_hash: None,
            lcl_seq: Some(5),
            lcl_hash: Some(Hash256::ZERO),
        };
        assert!(verify_chain_anchors(&[h1], &anchors).is_ok());
    }

    #[test]
    fn test_chain_anchors_lcl_seq_without_hash_is_noop() {
        // lcl_seq set but lcl_hash is None — the guard requires both
        let h1 = make_test_header(5, Hash256::ZERO);
        let anchors = ChainTrustAnchors {
            previous_ledger_hash: None,
            lcl_seq: Some(5),
            lcl_hash: None,
        };
        assert!(verify_chain_anchors(&[h1], &anchors).is_ok());
    }

    #[test]
    fn test_chain_anchors_both_bottom_and_lcl() {
        // Test both anchors simultaneously
        let prev_hash = Hash256::hash(b"prev");
        let h1 = make_test_header(10, prev_hash);
        let h1_hash = compute_header_hash(&h1).unwrap();
        let h2 = make_test_header(11, h1_hash);

        let anchors = ChainTrustAnchors {
            previous_ledger_hash: Some(prev_hash),
            lcl_seq: Some(10),
            lcl_hash: Some(h1_hash),
        };
        assert!(verify_chain_anchors(&[h1, h2], &anchors).is_ok());
    }

    // ── end verify_chain_anchors tests ──────────────────────────────────

    #[test]
    fn test_verify_tx_result_ordering_first_checkpoint() {
        use stellar_xdr::curr::{
            TransactionHistoryResultEntry, TransactionHistoryResultEntryExt, TransactionResultSet,
            VecM,
        };

        // Checkpoint 63: range is 0-63
        let entries = vec![
            TransactionHistoryResultEntry {
                ledger_seq: 1,
                tx_result_set: TransactionResultSet {
                    results: VecM::default(),
                },
                ext: TransactionHistoryResultEntryExt::V0,
            },
            TransactionHistoryResultEntry {
                ledger_seq: 63,
                tx_result_set: TransactionResultSet {
                    results: VecM::default(),
                },
                ext: TransactionHistoryResultEntryExt::V0,
            },
        ];
        assert!(verify_tx_result_ordering(&entries, 63).is_ok());
    }

    // ── verify_has_passphrase tests ─────────────────────────────────────

    fn make_test_has(passphrase: Option<&str>) -> HistoryArchiveState {
        use crate::archive_state::{HASBucketLevel, HASBucketNext};
        let level = HASBucketLevel {
            curr: "00".repeat(32),
            snap: "00".repeat(32),
            next: HASBucketNext {
                state: 0,
                output: None,
                curr: None,
                snap: None,
                shadow: None,
            },
        };
        HistoryArchiveState {
            version: 2,
            server: None,
            current_ledger: 63,
            network_passphrase: passphrase.map(|s| s.to_string()),
            current_buckets: vec![level.clone(); BUCKET_LIST_LEVELS],
            hot_archive_buckets: Some(vec![level; HOT_ARCHIVE_BUCKET_LIST_LEVELS]),
        }
    }

    #[test]
    fn test_verify_has_passphrase_matching() {
        let has = make_test_has(Some("Test SDF Network ; September 2015"));
        assert!(verify_has_passphrase(&has, "Test SDF Network ; September 2015").is_ok());
    }

    #[test]
    fn test_verify_has_passphrase_mismatch() {
        let has = make_test_has(Some("Test SDF Network ; September 2015"));
        let result = verify_has_passphrase(&has, "Public Global Stellar Network ; September 2015");
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("passphrase mismatch"));
    }

    #[test]
    fn test_verify_has_passphrase_none_skipped() {
        // Version 1 archives may have no passphrase — validation is skipped.
        let has = make_test_has(None);
        assert!(verify_has_passphrase(&has, "Test SDF Network ; September 2015").is_ok());
    }

    #[test]
    fn test_verify_has_passphrase_empty_skipped() {
        // Empty string passphrase is treated as absent — validation is skipped.
        let has = make_test_has(Some(""));
        assert!(verify_has_passphrase(&has, "Test SDF Network ; September 2015").is_ok());
    }

    // ── CATCHUP_SPEC §3.1: verify_has_structure tests ────────────────

    fn make_has_with_levels(
        n_levels: usize,
        version: u32,
        passphrase: Option<&str>,
    ) -> HistoryArchiveState {
        use crate::archive_state::{HASBucketLevel, HASBucketNext};
        let level = HASBucketLevel {
            curr: "00".repeat(32),
            snap: "00".repeat(32),
            next: HASBucketNext {
                state: 0,
                output: None,
                curr: None,
                snap: None,
                shadow: None,
            },
        };
        HistoryArchiveState {
            version,
            server: None,
            current_ledger: 63,
            network_passphrase: passphrase.map(|s| s.to_string()),
            current_buckets: vec![level; n_levels],
            hot_archive_buckets: None,
        }
    }

    #[test]
    fn test_verify_has_structure_valid_v1() {
        // Version 1 with 11 levels, no passphrase — valid.
        let has = make_has_with_levels(11, 1, None);
        assert!(verify_has_structure(&has).is_ok());
    }

    #[test]
    fn test_verify_has_structure_valid_v2_with_passphrase() {
        use crate::archive_state::{HASBucketLevel, HASBucketNext};
        // Version 2 with 11 levels, passphrase, and hotArchiveBuckets — valid.
        let level = HASBucketLevel {
            curr: "00".repeat(32),
            snap: "00".repeat(32),
            next: HASBucketNext {
                state: 0,
                output: None,
                curr: None,
                snap: None,
                shadow: None,
            },
        };
        let mut has = make_has_with_levels(11, 2, Some("Test SDF Network ; September 2015"));
        has.hot_archive_buckets = Some(vec![level; HOT_ARCHIVE_BUCKET_LIST_LEVELS]);
        assert!(verify_has_structure(&has).is_ok());
    }

    #[test]
    fn test_verify_has_structure_wrong_level_count() {
        // Not 11 levels — must fail.
        let has = make_has_with_levels(10, 1, None);
        let result = verify_has_structure(&has);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("bucket levels"));

        let has = make_has_with_levels(12, 1, None);
        assert!(verify_has_structure(&has).is_err());

        let has = make_has_with_levels(0, 1, None);
        assert!(verify_has_structure(&has).is_err());
    }

    #[test]
    fn test_verify_has_structure_unsupported_version() {
        // Version 0 and version 3 should fail.
        let has = make_has_with_levels(11, 0, None);
        let result = verify_has_structure(&has);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("version"));

        let has = make_has_with_levels(11, 3, None);
        assert!(verify_has_structure(&has).is_err());
    }

    #[test]
    fn test_verify_has_structure_v2_without_passphrase() {
        // Version 2 without networkPassphrase — must fail.
        let has = make_has_with_levels(11, 2, None);
        let result = verify_has_structure(&has);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("networkPassphrase"));
    }

    #[test]
    fn test_verify_has_structure_v1_without_passphrase_ok() {
        // Version 1 without passphrase — should be fine.
        let has = make_has_with_levels(11, 1, None);
        assert!(verify_has_structure(&has).is_ok());
    }

    #[test]
    fn test_verify_has_structure_v2_requires_hot_archive_buckets() {
        // Version 2 with passphrase but no hotArchiveBuckets — must fail.
        let has = make_has_with_levels(11, 2, Some("Test SDF Network ; September 2015"));
        let result = verify_has_structure(&has);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("hotArchiveBuckets"));
    }

    #[test]
    fn test_verify_has_structure_v2_wrong_hot_archive_count() {
        use crate::archive_state::{HASBucketLevel, HASBucketNext};
        // Version 2 with wrong number of hotArchiveBuckets — must fail.
        let level = HASBucketLevel {
            curr: "00".repeat(32),
            snap: "00".repeat(32),
            next: HASBucketNext {
                state: 0,
                output: None,
                curr: None,
                snap: None,
                shadow: None,
            },
        };
        let mut has = make_has_with_levels(11, 2, Some("Test SDF Network ; September 2015"));
        has.hot_archive_buckets = Some(vec![level.clone(); 5]); // wrong count
        let result = verify_has_structure(&has);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("hotArchiveBuckets"));
    }

    #[test]
    fn test_verify_has_structure_v2_empty_hot_archive_vec() {
        // Version 2 with Some(vec![]) — must fail (0 != 11).
        let mut has = make_has_with_levels(11, 2, Some("Test SDF Network ; September 2015"));
        has.hot_archive_buckets = Some(vec![]);
        let result = verify_has_structure(&has);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("hotArchiveBuckets"));
    }

    #[test]
    fn test_verify_has_structure_v2_correct_hot_archive_count() {
        use crate::archive_state::{HASBucketLevel, HASBucketNext};
        // Version 2 with correct 11 hotArchiveBuckets — valid.
        let level = HASBucketLevel {
            curr: "00".repeat(32),
            snap: "00".repeat(32),
            next: HASBucketNext {
                state: 0,
                output: None,
                curr: None,
                snap: None,
                shadow: None,
            },
        };
        let mut has = make_has_with_levels(11, 2, Some("Test SDF Network ; September 2015"));
        has.hot_archive_buckets = Some(vec![level; HOT_ARCHIVE_BUCKET_LIST_LEVELS]);
        assert!(verify_has_structure(&has).is_ok());
    }

    #[test]
    fn test_verify_has_structure_v1_with_hot_archive_buckets_rejected() {
        use crate::archive_state::{HASBucketLevel, HASBucketNext};
        // Version 1 with hotArchiveBuckets present — must fail.
        let level = HASBucketLevel {
            curr: "00".repeat(32),
            snap: "00".repeat(32),
            next: HASBucketNext {
                state: 0,
                output: None,
                curr: None,
                snap: None,
                shadow: None,
            },
        };
        let mut has = make_has_with_levels(11, 1, None);
        has.hot_archive_buckets = Some(vec![level; 11]);
        let result = verify_has_structure(&has);
        assert!(result.is_err());
        assert!(format!("{}", result.unwrap_err()).contains("version 1"));
    }

    // --- Direct producer tests for VerificationHashMismatch migration ---

    #[test]
    fn test_verify_bucket_hash_mismatch_typed() {
        let data = b"some bucket data";
        let wrong_hash = Hash256::ZERO;
        let err = verify_bucket_hash(data, &wrong_hash).unwrap_err();

        match err {
            HistoryError::VerificationHashMismatch(info) => {
                assert_eq!(info.kind(), crate::error::VerifyHashKind::Bucket);
                assert_eq!(info.ledger(), None);
                assert_eq!(info.expected(), wrong_hash);
                assert_eq!(info.actual(), Hash256::hash(data));
            }
            other => panic!("expected VerificationHashMismatch, got: {other}"),
        }
    }

    #[test]
    fn test_verify_bucket_hash_match_ok() {
        let data = b"some bucket data";
        let correct_hash = Hash256::hash(data);
        assert!(verify_bucket_hash(data, &correct_hash).is_ok());
    }

    #[test]
    fn test_verify_ledger_hash_mismatch_typed() {
        let mut header = make_test_header(42, Hash256::ZERO);
        // Set a bucket_list_hash that won't match the computed one.
        header.bucket_list_hash = Hash([0xAA; 32]);
        let computed_bucket_list_hash = Hash256::ZERO;

        let err = verify_ledger_hash(&header, &computed_bucket_list_hash).unwrap_err();

        match err {
            HistoryError::VerificationHashMismatch(info) => {
                assert_eq!(info.kind(), crate::error::VerifyHashKind::BucketList);
                assert_eq!(info.ledger(), Some(42));
                assert_eq!(info.expected(), Hash256::from(header.bucket_list_hash));
                assert_eq!(info.actual(), computed_bucket_list_hash);
            }
            other => panic!("expected VerificationHashMismatch, got: {other}"),
        }
    }

    #[test]
    fn test_verify_ledger_header_history_entry_mismatch_typed() {
        let header = make_test_header(100, Hash256::ZERO);
        let entry = LedgerHeaderHistoryEntry {
            hash: Hash([0xFF; 32]), // Wrong hash
            header,
            ext: LedgerHeaderHistoryEntryExt::V0,
        };

        let err = verify_ledger_header_history_entry(&entry).unwrap_err();

        match err {
            HistoryError::VerificationHashMismatch(info) => {
                assert_eq!(info.kind(), crate::error::VerifyHashKind::LedgerHeaderEntry);
                assert_eq!(info.ledger(), Some(100));
                assert_eq!(info.expected(), Hash256::from(Hash([0xFF; 32])));
                // actual should be the computed header hash
                let computed = compute_header_hash(&entry.header).unwrap();
                assert_eq!(info.actual(), computed);
            }
            other => panic!("expected VerificationHashMismatch, got: {other}"),
        }
    }

    #[test]
    fn test_verify_tx_result_set_mismatch_typed() {
        let mut header = make_test_header(50, Hash256::ZERO);
        header.tx_set_result_hash = Hash([0xBB; 32]); // Expected hash
        let wrong_xdr = b"wrong result set xdr";

        let err = verify_tx_result_set(&header, wrong_xdr).unwrap_err();

        match err {
            HistoryError::VerificationHashMismatch(info) => {
                assert_eq!(info.kind(), crate::error::VerifyHashKind::TxResultSet);
                assert_eq!(info.ledger(), Some(50));
                assert_eq!(info.expected(), Hash256::from(Hash([0xBB; 32])));
                assert_eq!(info.actual(), Hash256::hash(wrong_xdr));
            }
            other => panic!("expected VerificationHashMismatch, got: {other}"),
        }
    }

    #[test]
    fn test_verify_tx_result_set_genesis_exception() {
        let header = make_test_header(GENESIS_LEDGER_SEQ, Hash256::ZERO);
        // Empty result set at genesis should succeed regardless of hash.
        assert!(verify_tx_result_set(&header, &[]).is_ok());
    }

    #[test]
    fn test_verify_header_matches_trusted_mismatch_typed() {
        let downloaded = make_test_header(77, Hash256::ZERO);
        // Trusted header with same seq but different content.
        let mut trusted = make_test_header(77, Hash256::ZERO);
        trusted.total_coins = 999; // Different content → different hash

        let err = verify_header_matches_trusted(&downloaded, &trusted).unwrap_err();

        match err {
            HistoryError::VerificationHashMismatch(info) => {
                assert_eq!(info.kind(), crate::error::VerifyHashKind::TrustedHeader);
                assert_eq!(info.ledger(), Some(77));
                let trusted_hash = compute_header_hash(&trusted).unwrap();
                let downloaded_hash = compute_header_hash(&downloaded).unwrap();
                assert_eq!(info.expected(), trusted_hash);
                assert_eq!(info.actual(), downloaded_hash);
            }
            other => panic!("expected VerificationHashMismatch, got: {other}"),
        }
    }

    #[test]
    fn test_verify_header_matches_trusted_ok() {
        let header = make_test_header(77, Hash256::ZERO);
        assert!(verify_header_matches_trusted(&header, &header).is_ok());
    }

    // --- Precedence regression tests for verify_tx_result_ordering ---

    fn make_tx_result_entry(ledger_seq: u32) -> TransactionHistoryResultEntry {
        use stellar_xdr::curr::{TransactionHistoryResultEntryExt, TransactionResultSet};
        TransactionHistoryResultEntry {
            ledger_seq,
            tx_result_set: TransactionResultSet {
                results: VecM::default(),
            },
            ext: TransactionHistoryResultEntryExt::V0,
        }
    }

    #[test]
    fn test_ordering_violation_before_range_violation() {
        // Checkpoint 63 → range [1, 63].
        // Index 1: ordering violation (20 <= 30). Index 2: range violation (999 > 63).
        // Ordering violation comes first by position → ordering error reported.
        let entries = vec![
            make_tx_result_entry(30),
            make_tx_result_entry(20),  // ordering violation
            make_tx_result_entry(999), // range violation
        ];
        let err = verify_tx_result_ordering(&entries, 63).unwrap_err();
        assert!(
            err.to_string().contains("not strictly increasing"),
            "expected ordering error, got: {}",
            err
        );
    }

    #[test]
    fn test_range_violation_before_ordering_violation() {
        // Checkpoint 63 → range [1, 63].
        // Index 0: range violation (999 > 63). Index 2: ordering violation (5 <= 10).
        // Range violation comes first by position → range error reported.
        let entries = vec![
            make_tx_result_entry(999), // range violation
            make_tx_result_entry(10),
            make_tx_result_entry(5), // ordering violation
        ];
        let err = verify_tx_result_ordering(&entries, 63).unwrap_err();
        assert!(
            err.to_string().contains("outside checkpoint range"),
            "expected range error, got: {}",
            err
        );
    }

    #[test]
    fn test_same_entry_violates_both_range_wins() {
        // Checkpoint 63 → range [1, 63].
        // Entry with seq=0 after seq=50: out-of-range (0 < 1) AND non-increasing (0 <= 50).
        // Range is checked first in the loop body → range error wins.
        let entries = vec![
            make_tx_result_entry(50),
            make_tx_result_entry(0), // range: 0 < 1; ordering: 0 <= 50
        ];
        let err = verify_tx_result_ordering(&entries, 63).unwrap_err();
        assert!(
            err.to_string().contains("outside checkpoint range"),
            "expected range error, got: {}",
            err
        );
    }

    // Use the shared tracing capture helper (serialized via process-wide mutex).
    use crate::tracing_test_support::capture_events;

    #[test]
    fn test_verify_tx_result_set_mismatch_emits_tracing() {
        let mut header = make_test_header(42, Hash256::ZERO);
        header.tx_set_result_hash = Hash([0xAA; 32]);
        let wrong_xdr = b"not the right result set";

        let events = capture_events(|| {
            let _ = verify_tx_result_set(&header, wrong_xdr);
        });

        assert_eq!(events.len(), 1, "expected exactly one tracing event");
        let event = &events[0];
        assert!(
            event.message.contains("verification hash mismatch"),
            "unexpected message: {}",
            event.message
        );

        let field_map: std::collections::HashMap<&str, &str> = event
            .fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        assert_eq!(field_map.get("ledger_seq"), Some(&"42"));
        assert!(
            field_map.contains_key("expected_hash"),
            "missing expected_hash field"
        );
        assert!(
            field_map.contains_key("actual_hash"),
            "missing actual_hash field"
        );
        assert!(field_map.contains_key("kind"), "missing kind field");
    }

    #[test]
    fn test_verify_bucket_hash_mismatch_emits_tracing_no_ledger_seq() {
        let data = b"bucket content";
        let wrong_hash = Hash256::from(Hash([0xFF; 32]));

        let events = capture_events(|| {
            let _ = verify_bucket_hash(data, &wrong_hash);
        });

        assert_eq!(events.len(), 1, "expected exactly one tracing event");
        let event = &events[0];

        let field_map: std::collections::HashMap<&str, &str> = event
            .fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        // Bucket verification has no ledger context — ledger_seq should be absent.
        assert!(
            !field_map.contains_key("ledger_seq"),
            "ledger_seq should not be present for bucket hash verification"
        );
        assert!(
            field_map.contains_key("expected_hash"),
            "missing expected_hash field"
        );
        assert!(
            field_map.contains_key("actual_hash"),
            "missing actual_hash field"
        );
    }

    #[test]
    fn test_verify_tx_result_set_genesis_does_not_emit_tracing() {
        let header = make_test_header(GENESIS_LEDGER_SEQ, Hash256::ZERO);

        let events = capture_events(|| {
            // Genesis with empty result set should succeed without logging.
            let _ = verify_tx_result_set(&header, &[]);
        });

        assert!(
            events.is_empty(),
            "genesis fast-path should not emit any tracing events"
        );
    }

    // ─── Reverse-Walk Tests ─────────────────────────────────────────────────

    /// Helper: build a valid chain of headers with correct hash links.
    /// Returns headers with ledger_version set to the given version.
    fn make_chain(start_seq: u32, count: u32, version: u32) -> Vec<LedgerHeader> {
        let mut headers = Vec::with_capacity(count as usize);
        let mut prev_hash = Hash256::ZERO;
        for i in 0..count {
            let mut h = make_test_header(start_seq + i, prev_hash);
            h.ledger_version = version;
            prev_hash = compute_header_hash(&h).unwrap();
            headers.push(h);
        }
        headers
    }

    #[test]
    fn test_reverse_walk_valid_multi_checkpoint() {
        // Build a chain spanning 3 checkpoints: ledgers 1..=191
        // checkpoint_containing(1)=63, checkpoint_containing(64)=127, checkpoint_containing(128)=191
        let headers = make_chain(1, 191, 25);
        let config = ReverseWalkConfig {
            trust_source: TrustSource::None,
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let result = verify_reverse_walk(&headers, &config).unwrap();
        assert!(!result.fatal_failure);
    }

    #[test]
    fn test_reverse_walk_broken_cross_checkpoint_link() {
        // Build a valid chain, then tamper with the first header of the second
        // checkpoint so its previous_ledger_hash is wrong. This corrupts the
        // content of header at seq 64, making the link FROM seq 65 invalid.
        let mut headers = make_chain(1, 128, 25);
        // Tamper with header at seq 64 (first in second checkpoint).
        headers[63].previous_ledger_hash = Hash256::ZERO.into();

        let config = ReverseWalkConfig {
            trust_source: TrustSource::None,
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        // The error is detected at ledger 65, which tries to link back to
        // the tampered header 64.
        assert!(
            matches!(err, HistoryError::InvalidPreviousHash { ledger: 65 }),
            "expected InvalidPreviousHash at ledger 65, got: {err}"
        );
    }

    #[test]
    fn test_reverse_walk_sequence_gap() {
        let mut headers = make_chain(1, 10, 25);
        // Create a gap: change seq 5 to seq 7
        headers[4].ledger_seq = 7;

        let config = ReverseWalkConfig {
            trust_source: TrustSource::None,
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        assert!(matches!(
            err,
            HistoryError::InvalidSequence {
                expected: 5,
                got: 7
            }
        ));
    }

    #[test]
    fn test_reverse_walk_lcl_plus_one_mismatch_with_scp_trust() {
        // Build a valid chain starting at ledger 10 (LCL is 9).
        let headers = make_chain(10, 5, 25);
        let last_hash = compute_header_hash(headers.last().unwrap()).unwrap();

        // LCL hash doesn't match the first header's previous_ledger_hash
        let wrong_lcl_hash = Hash256([0xAA; 32]);
        let config = ReverseWalkConfig {
            trust_source: TrustSource::Scp {
                seq: 14,
                hash: last_hash,
            },
            lcl: Some((9, wrong_lcl_hash)),
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        assert!(
            matches!(err, HistoryError::FatalChainDisagreement),
            "expected FatalChainDisagreement, got: {err}"
        );
    }

    #[test]
    fn test_reverse_walk_lcl_plus_one_mismatch_no_trust() {
        // Same as above but with TrustSource::None — should be InvalidPreviousHash.
        let headers = make_chain(10, 5, 25);
        let wrong_lcl_hash = Hash256([0xAA; 32]);
        let config = ReverseWalkConfig {
            trust_source: TrustSource::None,
            lcl: Some((9, wrong_lcl_hash)),
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        assert!(
            matches!(err, HistoryError::InvalidPreviousHash { .. }),
            "expected InvalidPreviousHash, got: {err}"
        );
    }

    #[test]
    fn test_reverse_walk_lcl_matches() {
        // LCL hash matches the first header's previous_ledger_hash — no error.
        let headers = make_chain(10, 5, 25);
        let lcl_hash = Hash256::from(headers[0].previous_ledger_hash.clone());
        let config = ReverseWalkConfig {
            trust_source: TrustSource::None,
            lcl: Some((9, lcl_hash)),
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let result = verify_reverse_walk(&headers, &config).unwrap();
        assert!(!result.fatal_failure);
    }

    #[test]
    fn test_reverse_walk_lcl_seq_overlap_mismatch_with_scp_trust() {
        // §9.3 step 2c: a downloaded header sits exactly at lcl_seq, but its
        // content hash disagrees with the local LCL hash. With an SCP trust
        // anchor this is a fatal chain disagreement.
        //
        // The LCL seq (10) is the HIGHEST downloaded header, so step 2d
        // (lcl_seq + 1 == 11) never matches the range — this isolates step 2c.
        let headers = make_chain(6, 5, 25); // seqs 6..=10; headers.last() is at lcl_seq (10).
        let top = headers.last().unwrap();
        assert_eq!(top.ledger_seq, 10);
        let top_hash = compute_header_hash(top).unwrap();

        // Wrong hash for the header at lcl_seq (10).
        let wrong_lcl_hash = Hash256([0xAA; 32]);
        assert_ne!(wrong_lcl_hash, top_hash);

        let config = ReverseWalkConfig {
            trust_source: TrustSource::Scp {
                seq: 10,
                hash: top_hash,
            },
            lcl: Some((10, wrong_lcl_hash)),
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        assert!(
            matches!(err, HistoryError::FatalChainDisagreement),
            "expected FatalChainDisagreement, got: {err}"
        );
    }

    #[test]
    fn test_reverse_walk_lcl_seq_overlap_mismatch_no_trust() {
        // Same step-2c overlap mismatch but without SCP trust — retriable, so
        // it surfaces as InvalidPreviousHash rather than a fatal disagreement.
        let headers = make_chain(6, 5, 25); // seqs 6..=10; headers.last() is at lcl_seq (10).
        let top = headers.last().unwrap();
        assert_eq!(top.ledger_seq, 10);
        let wrong_lcl_hash = Hash256([0xAA; 32]);
        assert_ne!(wrong_lcl_hash, compute_header_hash(top).unwrap());

        let config = ReverseWalkConfig {
            trust_source: TrustSource::None,
            lcl: Some((10, wrong_lcl_hash)),
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        assert!(
            matches!(err, HistoryError::InvalidPreviousHash { .. }),
            "expected InvalidPreviousHash, got: {err}"
        );
    }

    #[test]
    fn test_reverse_walk_lcl_seq_overlap_matches() {
        // §9.3 step 2c: the header at lcl_seq has the CORRECT content hash as the
        // local LCL hash — no disagreement should be flagged.
        let headers = make_chain(6, 5, 25); // seqs 6..=10; headers.last() is at lcl_seq (10).
        let top = headers.last().unwrap();
        assert_eq!(top.ledger_seq, 10);
        let top_hash = compute_header_hash(top).unwrap();
        let config = ReverseWalkConfig {
            trust_source: TrustSource::Scp {
                seq: 10,
                hash: top_hash,
            },
            lcl: Some((10, top_hash)),
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let result = verify_reverse_walk(&headers, &config).unwrap();
        assert!(!result.fatal_failure);
    }

    #[test]
    fn test_reverse_walk_unsupported_version_high() {
        let headers = make_chain(1, 5, 30); // version 30 > max 26
        let config = ReverseWalkConfig {
            trust_source: TrustSource::None,
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        assert!(matches!(
            err,
            HistoryError::UnsupportedLedgerVersion {
                version: 30,
                min: 24,
                max: 26,
                ..
            }
        ));
    }

    #[test]
    fn test_reverse_walk_unsupported_version_low() {
        let headers = make_chain(1, 5, 20); // version 20 < min 24
        let config = ReverseWalkConfig {
            trust_source: TrustSource::None,
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        assert!(matches!(
            err,
            HistoryError::UnsupportedLedgerVersion {
                version: 20,
                min: 24,
                max: 26,
                ..
            }
        ));
    }

    /// Regression test for #2724: genesis headers have ledger_version=0 which must
    /// pass verification (mirrors begin_close's version-0 exception).
    #[test]
    fn test_reverse_walk_allows_genesis_protocol_version() {
        let headers = make_chain(1, 5, 0); // version 0 = genesis
        let config = ReverseWalkConfig {
            trust_source: TrustSource::None,
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let result = verify_reverse_walk(&headers, &config).unwrap();
        assert!(!result.fatal_failure);
    }

    #[test]
    fn test_reverse_walk_single_checkpoint() {
        // All headers within one checkpoint (1..=63)
        let headers = make_chain(1, 10, 25);
        let config = ReverseWalkConfig {
            trust_source: TrustSource::None,
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let result = verify_reverse_walk(&headers, &config).unwrap();
        assert!(!result.fatal_failure);
    }

    #[test]
    fn test_reverse_walk_partial_checkpoint() {
        // Start at ledger 50 (mid-checkpoint), end at ledger 80 (mid-next-checkpoint)
        let full_chain = make_chain(1, 80, 25);
        let partial = &full_chain[49..80]; // ledgers 50..=80
        let config = ReverseWalkConfig {
            trust_source: TrustSource::None,
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let result = verify_reverse_walk(partial, &config).unwrap();
        assert!(!result.fatal_failure);
    }

    #[test]
    fn test_reverse_walk_genesis_boundary() {
        // First header is ledger 1 — outgoing link seq is 0
        let headers = make_chain(1, 5, 25);
        let config = ReverseWalkConfig {
            trust_source: TrustSource::None,
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let result = verify_reverse_walk(&headers, &config).unwrap();
        assert!(!result.fatal_failure);
    }

    #[test]
    fn test_reverse_walk_trust_anchor_mismatch() {
        let headers = make_chain(1, 10, 25);
        let wrong_hash = Hash256([0xFF; 32]);
        let config = ReverseWalkConfig {
            trust_source: TrustSource::Scp {
                seq: 10, // last header
                hash: wrong_hash,
            },
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        assert!(
            matches!(err, HistoryError::VerificationHashMismatch(_)),
            "expected VerificationHashMismatch, got: {err}"
        );
    }

    #[test]
    fn test_reverse_walk_trust_anchor_valid() {
        let headers = make_chain(1, 10, 25);
        let last_hash = compute_header_hash(&headers[9]).unwrap();
        let config = ReverseWalkConfig {
            trust_source: TrustSource::Scp {
                seq: 10,
                hash: last_hash,
            },
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let result = verify_reverse_walk(&headers, &config).unwrap();
        assert!(!result.fatal_failure);
    }

    #[test]
    fn test_reverse_walk_empty_headers() {
        let config = ReverseWalkConfig {
            trust_source: TrustSource::None,
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let result = verify_reverse_walk(&[], &config).unwrap();
        assert!(!result.fatal_failure);
    }

    #[test]
    fn test_reverse_walk_trust_anchor_seq_beyond_range() {
        // TrustSource::Scp points to seq 20, but headers only go up to seq 10.
        // This should error because the trust anchor can't be verified.
        let headers = make_chain(1, 10, 25);
        let config = ReverseWalkConfig {
            trust_source: TrustSource::Scp {
                seq: 20,
                hash: Hash256([0xAA; 32]),
            },
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        assert!(
            matches!(
                err,
                HistoryError::InvalidSequence {
                    expected: 20,
                    got: 10
                }
            ),
            "expected InvalidSequence for seq mismatch, got: {err}"
        );
    }

    #[test]
    fn test_reverse_walk_trust_anchor_seq_within_range() {
        // TrustSource::Scp points to seq 5, but headers go up to seq 10.
        // Should verify the header at seq 5 against the trust hash.
        let headers = make_chain(1, 10, 25);
        let hash_at_5 = compute_header_hash(&headers[4]).unwrap();
        let config = ReverseWalkConfig {
            trust_source: TrustSource::Scp {
                seq: 5,
                hash: hash_at_5,
            },
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let result = verify_reverse_walk(&headers, &config).unwrap();
        assert!(!result.fatal_failure);
    }

    #[test]
    fn test_reverse_walk_trust_anchor_seq_within_range_wrong_hash() {
        // TrustSource::Scp points to seq 5, but with wrong hash.
        let headers = make_chain(1, 10, 25);
        let config = ReverseWalkConfig {
            trust_source: TrustSource::Scp {
                seq: 5,
                hash: Hash256([0xBB; 32]),
            },
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        assert!(
            matches!(err, HistoryError::VerificationHashMismatch(_)),
            "expected VerificationHashMismatch, got: {err}"
        );
    }

    #[test]
    fn test_reverse_walk_trust_anchor_in_lower_checkpoint_group() {
        // Multi-checkpoint: headers span ledgers 1-191 (3 checkpoint groups:
        // 1-63, 64-127, 128-191). Trust anchor points to seq 100 which is
        // in the second group. This tests that the trust anchor propagates
        // past the highest group and is verified in the correct group.
        let headers = make_chain(1, 191, 25);
        let hash_at_100 = compute_header_hash(&headers[99]).unwrap(); // index 99 = seq 100
        let config = ReverseWalkConfig {
            trust_source: TrustSource::Scp {
                seq: 100,
                hash: hash_at_100,
            },
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let result = verify_reverse_walk(&headers, &config).unwrap();
        assert!(!result.fatal_failure);
    }

    #[test]
    fn test_reverse_walk_trust_anchor_in_lower_checkpoint_group_wrong_hash() {
        // Same as above but with wrong hash — should detect mismatch.
        let headers = make_chain(1, 191, 25);
        let config = ReverseWalkConfig {
            trust_source: TrustSource::Scp {
                seq: 100,
                hash: Hash256([0xCC; 32]),
            },
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        assert!(
            matches!(err, HistoryError::VerificationHashMismatch(_)),
            "expected VerificationHashMismatch, got: {err}"
        );
    }

    #[test]
    fn test_reverse_walk_cross_checkpoint_link_enforced_with_lower_trust_anchor() {
        // Regression: even when trust anchor is in a lower group, cross-checkpoint
        // linking between higher groups must still be verified.
        //
        // Strategy: build a valid chain 1-127, and a separate unlinked chain
        // 128-191. The internal links within each segment are valid, but the
        // cross-checkpoint link (hash of header 127 vs 128.previous_ledger_hash)
        // will fail.
        let lower = make_chain(1, 127, 25);
        // Build upper chain starting at 128 with a different genesis seed
        // so its first header's previous_ledger_hash won't match hash(127).
        let mut upper = make_chain(128, 191, 25);
        // Tamper: set first header's previous_ledger_hash to garbage so it
        // doesn't match hash(lower[126]).
        upper[0].previous_ledger_hash = stellar_xdr::curr::Hash([0xDD; 32]);

        let mut headers: Vec<_> = lower.into_iter().chain(upper).collect();
        // Re-compute internal chain for upper group: header 129's prev must
        // match hash of (tampered) header 128, etc. Use make_chain's approach.
        for i in 128..headers.len() {
            let prev_hash = compute_header_hash(&headers[i - 1]).unwrap();
            headers[i].previous_ledger_hash = stellar_xdr::curr::Hash(prev_hash.0);
        }

        let hash_at_50 = compute_header_hash(&headers[49]).unwrap();
        let config = ReverseWalkConfig {
            trust_source: TrustSource::Scp {
                seq: 50,
                hash: hash_at_50,
            },
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        // Cross-checkpoint link: outgoing from group 128-191 is
        // (127, headers[127].previous_ledger_hash) which is garbage [0xDD..],
        // but hash(headers[126]) is the real hash of ledger 127.
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        assert!(
            matches!(err, HistoryError::VerificationHashMismatch(_)),
            "expected cross-checkpoint hash mismatch, got: {err}"
        );
    }

    #[test]
    fn test_reverse_walk_trust_anchor_below_lowest_header() {
        // Trust anchor at seq 5, but headers start at seq 10.
        // The anchor can never be verified — should error.
        let headers = make_chain(10, 20, 25);
        let config = ReverseWalkConfig {
            trust_source: TrustSource::Scp {
                seq: 5,
                hash: Hash256([0xEE; 32]),
            },
            lcl: None,
            max_supported_version: 26,
            min_supported_version: 24,
        };
        let err = verify_reverse_walk(&headers, &config).unwrap_err();
        assert!(
            matches!(
                err,
                HistoryError::InvalidSequence {
                    expected: 5,
                    got: 10
                }
            ),
            "expected InvalidSequence for below-range anchor, got: {err}"
        );
    }
}
