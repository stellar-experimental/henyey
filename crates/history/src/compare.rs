//! Checkpoint comparison between two history archives.
//!
//! Compares a checkpoint published by one archive (typically a local henyey
//! validator) against a reference archive (typically an SDF validator archive)
//! and reports any differences in:
//!
//! - **HAS (History Archive State)**: bucket list hashes at every level
//! - **Ledger headers**: every field of every header in the checkpoint
//! - **Transactions**: transaction envelopes per ledger
//! - **Results**: transaction result codes and fees
//!
//! SCP messages are intentionally skipped because different validators sign
//! with different keys, so the envelopes will always differ.
//!
//! Bucket files are compared by hash only (via the HAS), not byte-for-byte.

use std::fmt;

use stellar_xdr::curr::{
    LedgerHeaderHistoryEntry, TransactionHistoryEntry, TransactionHistoryResultEntry, WriteXdr,
};

use crate::archive::HistoryArchive;
use crate::archive_state::HistoryArchiveState;
use crate::error::HistoryError;

/// Number of leading hex characters to display when showing hash prefixes.
const HASH_DISPLAY_PREFIX_LEN: usize = 16;

// ============================================================================
// Types
// ============================================================================

/// A single mismatch found during checkpoint comparison.
#[derive(Debug, Clone)]
pub struct Mismatch {
    /// Which category the mismatch belongs to.
    pub category: Category,
    /// Human-readable description of the mismatch.
    pub detail: String,
}

/// Category of compared data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Has,
    LedgerHeaders,
    Transactions,
    Results,
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Category::Has => write!(f, "HAS"),
            Category::LedgerHeaders => write!(f, "ledger-headers"),
            Category::Transactions => write!(f, "transactions"),
            Category::Results => write!(f, "results"),
        }
    }
}

/// Result of comparing a single checkpoint between two archives.
#[derive(Debug)]
pub struct CheckpointComparison {
    /// The checkpoint ledger that was compared.
    pub checkpoint: u32,
    /// All mismatches found (empty = perfect match).
    pub mismatches: Vec<Mismatch>,
}

impl CheckpointComparison {
    /// Returns true if the two archives match perfectly for this checkpoint.
    pub fn is_match(&self) -> bool {
        self.mismatches.is_empty()
    }

    /// Number of mismatches found.
    pub fn mismatch_count(&self) -> usize {
        self.mismatches.len()
    }

    /// Print a human-readable summary to stdout.
    pub fn print_summary(&self) {
        if self.is_match() {
            println!("Checkpoint {} MATCHES: all data identical", self.checkpoint);
            return;
        }

        println!(
            "Checkpoint {} DIFFERS: {} mismatch(es) found",
            self.checkpoint,
            self.mismatches.len()
        );
        for (i, m) in self.mismatches.iter().enumerate() {
            println!("  [{}] {}: {}", i + 1, m.category, m.detail);
        }
    }
}

// ============================================================================
// Comparison logic
// ============================================================================

/// Compare a checkpoint between a local archive and a reference archive.
///
/// Downloads the checkpoint data from both archives and performs a typed,
/// field-by-field comparison. Returns a [`CheckpointComparison`] with all
/// mismatches found.
///
/// SCP messages are skipped (different validators produce different envelopes).
/// Buckets are compared by hash only (via the HAS bucket list hashes).
pub async fn compare_checkpoint(
    local: &HistoryArchive,
    reference: &HistoryArchive,
    checkpoint: u32,
) -> Result<CheckpointComparison, HistoryError> {
    let mut mismatches = Vec::new();

    // --- HAS ---
    let local_has = local.fetch_checkpoint_has(checkpoint).await?;
    let ref_has = reference.fetch_checkpoint_has(checkpoint).await?;
    mismatches.extend(compare_has(&local_has, &ref_has));

    // --- Ledger headers ---
    let local_headers = local.fetch_ledger_headers(checkpoint).await?;
    let ref_headers = reference.fetch_ledger_headers(checkpoint).await?;
    mismatches.extend(compare_ledger_headers(&local_headers, &ref_headers));

    // --- Transactions ---
    let local_txs = local.fetch_transactions(checkpoint).await?;
    let ref_txs = reference.fetch_transactions(checkpoint).await?;
    mismatches.extend(compare_entries(&local_txs, &ref_txs));

    // --- Results ---
    let local_results = local.fetch_results(checkpoint).await?;
    let ref_results = reference.fetch_results(checkpoint).await?;
    mismatches.extend(compare_entries(&local_results, &ref_results));

    Ok(CheckpointComparison {
        checkpoint,
        mismatches,
    })
}

// ============================================================================
// HAS comparison
// ============================================================================

fn compare_has(local: &HistoryArchiveState, reference: &HistoryArchiveState) -> Vec<Mismatch> {
    let mut out = Vec::new();
    if local.current_ledger != reference.current_ledger {
        out.push(Mismatch {
            category: Category::Has,
            detail: format!(
                "currentLedger: local={} reference={}",
                local.current_ledger, reference.current_ledger
            ),
        });
    }

    // Compare live bucket list hashes level by level.
    let local_levels = &local.current_buckets;
    let ref_levels = &reference.current_buckets;

    if local_levels.len() != ref_levels.len() {
        out.push(Mismatch {
            category: Category::Has,
            detail: format!(
                "bucket level count: local={} reference={}",
                local_levels.len(),
                ref_levels.len()
            ),
        });
        return out;
    }

    for (i, (l, r)) in local_levels.iter().zip(ref_levels.iter()).enumerate() {
        if l.curr != r.curr {
            out.push(Mismatch {
                category: Category::Has,
                detail: format!(
                    "bucket level {} curr: local={} reference={}",
                    i,
                    &l.curr[..HASH_DISPLAY_PREFIX_LEN.min(l.curr.len())],
                    &r.curr[..HASH_DISPLAY_PREFIX_LEN.min(r.curr.len())],
                ),
            });
        }
        if l.snap != r.snap {
            out.push(Mismatch {
                category: Category::Has,
                detail: format!(
                    "bucket level {} snap: local={} reference={}",
                    i,
                    &l.snap[..HASH_DISPLAY_PREFIX_LEN.min(l.snap.len())],
                    &r.snap[..HASH_DISPLAY_PREFIX_LEN.min(r.snap.len())],
                ),
            });
        }
    }

    // Compare hot archive bucket hashes if present.
    match (&local.hot_archive_buckets, &reference.hot_archive_buckets) {
        (Some(l_hot), Some(r_hot)) => {
            if l_hot.len() != r_hot.len() {
                out.push(Mismatch {
                    category: Category::Has,
                    detail: format!(
                        "hot archive bucket level count: local={} reference={}",
                        l_hot.len(),
                        r_hot.len()
                    ),
                });
                return out;
            }
            for (i, (l, r)) in l_hot.iter().zip(r_hot.iter()).enumerate() {
                if l.curr != r.curr {
                    out.push(Mismatch {
                        category: Category::Has,
                        detail: format!(
                            "hot archive bucket level {} curr: local={} reference={}",
                            i,
                            &l.curr[..HASH_DISPLAY_PREFIX_LEN.min(l.curr.len())],
                            &r.curr[..HASH_DISPLAY_PREFIX_LEN.min(r.curr.len())],
                        ),
                    });
                }
                if l.snap != r.snap {
                    out.push(Mismatch {
                        category: Category::Has,
                        detail: format!(
                            "hot archive bucket level {} snap: local={} reference={}",
                            i,
                            &l.snap[..HASH_DISPLAY_PREFIX_LEN.min(l.snap.len())],
                            &r.snap[..HASH_DISPLAY_PREFIX_LEN.min(r.snap.len())],
                        ),
                    });
                }
            }
        }
        (None, None) => {}
        (Some(_), None) => {
            out.push(Mismatch {
                category: Category::Has,
                detail: "hot archive buckets: present in local, missing in reference".to_string(),
            });
        }
        (None, Some(_)) => {
            out.push(Mismatch {
                category: Category::Has,
                detail: "hot archive buckets: missing in local, present in reference".to_string(),
            });
        }
    }
    out
}

// ============================================================================
// Ledger header comparison
// ============================================================================

fn compare_ledger_headers(
    local: &[LedgerHeaderHistoryEntry],
    reference: &[LedgerHeaderHistoryEntry],
) -> Vec<Mismatch> {
    let mut out = Vec::new();
    if local.len() != reference.len() {
        out.push(Mismatch {
            category: Category::LedgerHeaders,
            detail: format!(
                "header count: local={} reference={}",
                local.len(),
                reference.len()
            ),
        });
        // Still compare what we can.
    }

    let min_len = local.len().min(reference.len());
    for i in 0..min_len {
        let l = &local[i];
        let r = &reference[i];

        // Compare the entry hash (quick check).
        if l.hash != r.hash {
            out.push(Mismatch {
                category: Category::LedgerHeaders,
                detail: format!("ledger {}: hash mismatch", l.header.ledger_seq,),
            });
        }

        // Compare the full header by XDR serialization.
        let l_xdr = l.header.to_xdr(stellar_xdr::curr::Limits::none());
        let r_xdr = r.header.to_xdr(stellar_xdr::curr::Limits::none());
        match (l_xdr, r_xdr) {
            (Ok(l_bytes), Ok(r_bytes)) if l_bytes != r_bytes => {
                // Identify which fields differ.
                let h_l = &l.header;
                let h_r = &r.header;
                let mut diffs = Vec::new();

                if h_l.ledger_version != h_r.ledger_version {
                    diffs.push(format!(
                        "ledger_version: {}!={}",
                        h_l.ledger_version, h_r.ledger_version
                    ));
                }
                if h_l.previous_ledger_hash != h_r.previous_ledger_hash {
                    diffs.push("previous_ledger_hash".to_string());
                }
                if h_l.scp_value != h_r.scp_value {
                    diffs.push("scp_value".to_string());
                }
                if h_l.tx_set_result_hash != h_r.tx_set_result_hash {
                    diffs.push("tx_set_result_hash".to_string());
                }
                if h_l.bucket_list_hash != h_r.bucket_list_hash {
                    diffs.push("bucket_list_hash".to_string());
                }
                if h_l.ledger_seq != h_r.ledger_seq {
                    diffs.push(format!(
                        "ledger_seq: {}!={}",
                        h_l.ledger_seq, h_r.ledger_seq
                    ));
                }
                if h_l.total_coins != h_r.total_coins {
                    diffs.push(format!(
                        "total_coins: {}!={}",
                        h_l.total_coins, h_r.total_coins
                    ));
                }
                if h_l.fee_pool != h_r.fee_pool {
                    diffs.push(format!("fee_pool: {}!={}", h_l.fee_pool, h_r.fee_pool));
                }
                if h_l.base_fee != h_r.base_fee {
                    diffs.push(format!("base_fee: {}!={}", h_l.base_fee, h_r.base_fee));
                }
                if h_l.base_reserve != h_r.base_reserve {
                    diffs.push(format!(
                        "base_reserve: {}!={}",
                        h_l.base_reserve, h_r.base_reserve
                    ));
                }
                if h_l.max_tx_set_size != h_r.max_tx_set_size {
                    diffs.push(format!(
                        "max_tx_set_size: {}!={}",
                        h_l.max_tx_set_size, h_r.max_tx_set_size
                    ));
                }
                if h_l.inflation_seq != h_r.inflation_seq {
                    diffs.push(format!(
                        "inflation_seq: {}!={}",
                        h_l.inflation_seq, h_r.inflation_seq
                    ));
                }
                if h_l.id_pool != h_r.id_pool {
                    diffs.push(format!("id_pool: {}!={}", h_l.id_pool, h_r.id_pool));
                }
                if h_l.ext != h_r.ext {
                    diffs.push("ext".to_string());
                }

                if !diffs.is_empty() {
                    out.push(Mismatch {
                        category: Category::LedgerHeaders,
                        detail: format!(
                            "ledger {}: fields differ: {}",
                            h_l.ledger_seq,
                            diffs.join(", ")
                        ),
                    });
                }
            }
            _ => {}
        }
    }

    // Report any extra headers on either side.
    if local.len() > min_len {
        out.push(Mismatch {
            category: Category::LedgerHeaders,
            detail: format!(
                "local has {} extra header(s) beyond reference",
                local.len() - min_len
            ),
        });
    }
    if reference.len() > min_len {
        out.push(Mismatch {
            category: Category::LedgerHeaders,
            detail: format!(
                "reference has {} extra header(s) beyond local",
                reference.len() - min_len
            ),
        });
    }
    out
}

// ============================================================================
// Transaction & result comparison
// ============================================================================

/// Trait for history entry types that can be compared by XDR serialization.
trait ComparableEntry {
    fn ledger_seq(&self) -> u32;
    fn payload_xdr(&self) -> std::result::Result<Vec<u8>, stellar_xdr::curr::Error>;
    fn category() -> Category;
    fn payload_name() -> &'static str;
}

impl ComparableEntry for TransactionHistoryEntry {
    fn ledger_seq(&self) -> u32 {
        self.ledger_seq
    }
    fn payload_xdr(&self) -> std::result::Result<Vec<u8>, stellar_xdr::curr::Error> {
        self.tx_set.to_xdr(stellar_xdr::curr::Limits::none())
    }
    fn category() -> Category {
        Category::Transactions
    }
    fn payload_name() -> &'static str {
        "tx_set"
    }
}

impl ComparableEntry for TransactionHistoryResultEntry {
    fn ledger_seq(&self) -> u32 {
        self.ledger_seq
    }
    fn payload_xdr(&self) -> std::result::Result<Vec<u8>, stellar_xdr::curr::Error> {
        self.tx_result_set.to_xdr(stellar_xdr::curr::Limits::none())
    }
    fn category() -> Category {
        Category::Results
    }
    fn payload_name() -> &'static str {
        "tx_result_set"
    }
}

/// Validates that entries are strictly increasing by `ledger_seq`.
/// Returns the index of the first violation, or `None` if valid.
fn find_ordering_violation<T: ComparableEntry>(entries: &[T]) -> Option<usize> {
    entries
        .windows(2)
        .position(|w| w[1].ledger_seq() <= w[0].ledger_seq())
        .map(|pos| pos + 1)
}

/// Compares two entry slices by `ledger_seq` using a merge-join.
///
/// Both slices must be strictly increasing by `ledger_seq`. If either is not,
/// a mismatch is reported and comparison is skipped. Entries present in one
/// side but not the other are reported individually by ledger number. Payload
/// differences and serialization failures are reported explicitly.
fn compare_entries<T: ComparableEntry>(local: &[T], reference: &[T]) -> Vec<Mismatch> {
    let mut out = Vec::new();
    let category = T::category();

    // Validate strict ordering on both sides.
    if let Some(idx) = find_ordering_violation(local) {
        out.push(Mismatch {
            category,
            detail: format!(
                "local entries not strictly ordered by ledger_seq (at index {})",
                idx
            ),
        });
        return out;
    }
    if let Some(idx) = find_ordering_violation(reference) {
        out.push(Mismatch {
            category,
            detail: format!(
                "reference entries not strictly ordered by ledger_seq (at index {})",
                idx
            ),
        });
        return out;
    }

    // Merge-join on ledger_seq.
    let mut i = 0;
    let mut j = 0;
    while i < local.len() && j < reference.len() {
        let l = &local[i];
        let r = &reference[j];
        match l.ledger_seq().cmp(&r.ledger_seq()) {
            std::cmp::Ordering::Equal => {
                // Compare payloads, handling serialization errors explicitly.
                match (l.payload_xdr(), r.payload_xdr()) {
                    (Ok(l_bytes), Ok(r_bytes)) => {
                        if l_bytes != r_bytes {
                            out.push(Mismatch {
                                category,
                                detail: format!(
                                    "ledger {}: {} differs (local {} bytes, reference {} bytes)",
                                    l.ledger_seq(),
                                    T::payload_name(),
                                    l_bytes.len(),
                                    r_bytes.len(),
                                ),
                            });
                        }
                    }
                    (Err(e), _) => {
                        out.push(Mismatch {
                            category,
                            detail: format!(
                                "ledger {}: local {} serialization error: {}",
                                l.ledger_seq(),
                                T::payload_name(),
                                e
                            ),
                        });
                    }
                    (_, Err(e)) => {
                        out.push(Mismatch {
                            category,
                            detail: format!(
                                "ledger {}: reference {} serialization error: {}",
                                r.ledger_seq(),
                                T::payload_name(),
                                e
                            ),
                        });
                    }
                }
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => {
                out.push(Mismatch {
                    category,
                    detail: format!(
                        "ledger {}: {} entry present in local but missing in reference",
                        l.ledger_seq(),
                        T::payload_name(),
                    ),
                });
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                out.push(Mismatch {
                    category,
                    detail: format!(
                        "ledger {}: {} entry present in reference but missing in local",
                        r.ledger_seq(),
                        T::payload_name(),
                    ),
                });
                j += 1;
            }
        }
    }

    // Report remaining entries on either side.
    while i < local.len() {
        out.push(Mismatch {
            category,
            detail: format!(
                "ledger {}: {} entry present in local but missing in reference",
                local[i].ledger_seq(),
                T::payload_name(),
            ),
        });
        i += 1;
    }
    while j < reference.len() {
        out.push(Mismatch {
            category,
            detail: format!(
                "ledger {}: {} entry present in reference but missing in local",
                reference[j].ledger_seq(),
                T::payload_name(),
            ),
        });
        j += 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_state::{HASBucketLevel, HASBucketNext};
    use stellar_xdr::curr::{
        Hash, TransactionHistoryEntryExt, TransactionHistoryResultEntryExt, TransactionResultSet,
        TransactionSet,
    };

    // ========================================================================
    // Helpers for compare_entries tests
    // ========================================================================

    fn make_tx_entry(ledger_seq: u32) -> TransactionHistoryEntry {
        TransactionHistoryEntry {
            ledger_seq,
            tx_set: TransactionSet {
                previous_ledger_hash: Hash([0u8; 32]),
                txs: stellar_xdr::curr::VecM::default(),
            },
            ext: TransactionHistoryEntryExt::V0,
        }
    }

    fn make_tx_entry_with_hash(ledger_seq: u32, hash_byte: u8) -> TransactionHistoryEntry {
        TransactionHistoryEntry {
            ledger_seq,
            tx_set: TransactionSet {
                previous_ledger_hash: Hash([hash_byte; 32]),
                txs: stellar_xdr::curr::VecM::default(),
            },
            ext: TransactionHistoryEntryExt::V0,
        }
    }

    fn make_result_entry(ledger_seq: u32) -> TransactionHistoryResultEntry {
        TransactionHistoryResultEntry {
            ledger_seq,
            tx_result_set: TransactionResultSet {
                results: stellar_xdr::curr::VecM::default(),
            },
            ext: TransactionHistoryResultEntryExt::default(),
        }
    }

    fn make_result_entry_with_results(
        ledger_seq: u32,
        num_results: usize,
    ) -> TransactionHistoryResultEntry {
        // Create entries with different content by varying the number of
        // (empty) result pairs.
        let results: Vec<stellar_xdr::curr::TransactionResultPair> = (0..num_results)
            .map(|_| stellar_xdr::curr::TransactionResultPair {
                transaction_hash: stellar_xdr::curr::Hash([0u8; 32]),
                result: stellar_xdr::curr::TransactionResult {
                    fee_charged: 100,
                    result: stellar_xdr::curr::TransactionResultResult::TxSuccess(
                        stellar_xdr::curr::VecM::default(),
                    ),
                    ext: stellar_xdr::curr::TransactionResultExt::V0,
                },
            })
            .collect();
        TransactionHistoryResultEntry {
            ledger_seq,
            tx_result_set: TransactionResultSet {
                results: results.try_into().unwrap(),
            },
            ext: TransactionHistoryResultEntryExt::default(),
        }
    }

    fn make_has(ledger: u32, curr_hashes: &[&str]) -> HistoryArchiveState {
        let levels: Vec<HASBucketLevel> = curr_hashes
            .iter()
            .map(|h| HASBucketLevel {
                curr: h.to_string(),
                snap: "0000000000000000000000000000000000000000000000000000000000000000"
                    .to_string(),
                next: HASBucketNext::default(),
            })
            .collect();
        HistoryArchiveState {
            version: 2,
            server: None,
            current_ledger: ledger,
            network_passphrase: None,
            current_buckets: levels,
            hot_archive_buckets: None,
        }
    }

    #[test]
    fn test_has_match() {
        let has = make_has(63, &["aaaa", "bbbb"]);
        let mismatches = compare_has(&has, &has.clone());
        assert!(mismatches.is_empty());
    }

    #[test]
    fn test_has_ledger_mismatch() {
        let a = make_has(63, &["aaaa"]);
        let b = make_has(127, &["aaaa"]);
        let mismatches = compare_has(&a, &b);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0].detail.contains("currentLedger"));
    }

    #[test]
    fn test_has_bucket_mismatch() {
        let a = make_has(63, &["aaaa"]);
        let b = make_has(63, &["bbbb"]);
        let mismatches = compare_has(&a, &b);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0].detail.contains("curr"));
    }

    #[test]
    fn test_has_level_count_mismatch() {
        let a = make_has(63, &["aaaa", "bbbb"]);
        let b = make_has(63, &["aaaa"]);
        let mismatches = compare_has(&a, &b);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0].detail.contains("bucket level count"));
    }

    // ========================================================================
    // compare_entries tests — TransactionHistoryEntry
    // ========================================================================

    #[test]
    fn test_compare_entries_identical() {
        let entries = vec![make_tx_entry(100), make_tx_entry(101), make_tx_entry(102)];
        let mismatches = compare_entries(&entries, &entries);
        assert!(mismatches.is_empty());
    }

    #[test]
    fn test_compare_entries_both_empty() {
        let empty: Vec<TransactionHistoryEntry> = vec![];
        let mismatches = compare_entries(&empty, &empty);
        assert!(mismatches.is_empty());
    }

    #[test]
    fn test_compare_entries_local_has_extra() {
        let local = vec![make_tx_entry(100), make_tx_entry(101), make_tx_entry(102)];
        let reference = vec![make_tx_entry(100), make_tx_entry(102)];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(
            mismatches[0].detail,
            "ledger 101: tx_set entry present in local but missing in reference"
        );
    }

    #[test]
    fn test_compare_entries_reference_has_extra() {
        let local = vec![make_tx_entry(100), make_tx_entry(102)];
        let reference = vec![make_tx_entry(100), make_tx_entry(101), make_tx_entry(102)];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(
            mismatches[0].detail,
            "ledger 101: tx_set entry present in reference but missing in local"
        );
    }

    #[test]
    fn test_compare_entries_interleaved_missing() {
        // local has ledgers 100, 102, 104; reference has 101, 102, 103
        let local = vec![make_tx_entry(100), make_tx_entry(102), make_tx_entry(104)];
        let reference = vec![make_tx_entry(101), make_tx_entry(102), make_tx_entry(103)];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 4);
        assert_eq!(
            mismatches[0].detail,
            "ledger 100: tx_set entry present in local but missing in reference"
        );
        assert_eq!(
            mismatches[1].detail,
            "ledger 101: tx_set entry present in reference but missing in local"
        );
        // ledger 102 matches (no mismatch)
        assert_eq!(
            mismatches[2].detail,
            "ledger 103: tx_set entry present in reference but missing in local"
        );
        assert_eq!(
            mismatches[3].detail,
            "ledger 104: tx_set entry present in local but missing in reference"
        );
    }

    #[test]
    fn test_compare_entries_payload_mismatch() {
        let local = vec![make_tx_entry_with_hash(100, 0xAA)];
        let reference = vec![make_tx_entry_with_hash(100, 0xBB)];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0]
            .detail
            .starts_with("ledger 100: tx_set differs"));
    }

    #[test]
    fn test_compare_entries_interleaved_missing_and_payload_mismatch() {
        // Proves merge-join re-synchronizes after gaps and still finds payload diff.
        let local = vec![
            make_tx_entry(100),
            make_tx_entry(102),
            make_tx_entry_with_hash(104, 0xAA),
        ];
        let reference = vec![
            make_tx_entry(101),
            make_tx_entry(102),
            make_tx_entry_with_hash(104, 0xBB),
        ];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 3);
        assert!(mismatches[0].detail.contains("ledger 100"));
        assert!(mismatches[0]
            .detail
            .contains("present in local but missing in reference"));
        assert!(mismatches[1].detail.contains("ledger 101"));
        assert!(mismatches[1]
            .detail
            .contains("present in reference but missing in local"));
        // ledger 102 matches
        assert!(mismatches[2]
            .detail
            .starts_with("ledger 104: tx_set differs"));
    }

    #[test]
    fn test_compare_entries_trailing_on_local() {
        let local = vec![make_tx_entry(100), make_tx_entry(101), make_tx_entry(102)];
        let reference = vec![make_tx_entry(100)];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 2);
        assert!(mismatches[0].detail.contains("ledger 101"));
        assert!(mismatches[0]
            .detail
            .contains("present in local but missing in reference"));
        assert!(mismatches[1].detail.contains("ledger 102"));
        assert!(mismatches[1]
            .detail
            .contains("present in local but missing in reference"));
    }

    #[test]
    fn test_compare_entries_trailing_on_reference() {
        let local = vec![make_tx_entry(100)];
        let reference = vec![make_tx_entry(100), make_tx_entry(101), make_tx_entry(102)];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 2);
        assert!(mismatches[0].detail.contains("ledger 101"));
        assert!(mismatches[0]
            .detail
            .contains("present in reference but missing in local"));
        assert!(mismatches[1].detail.contains("ledger 102"));
        assert!(mismatches[1]
            .detail
            .contains("present in reference but missing in local"));
    }

    #[test]
    fn test_compare_entries_empty_vs_nonempty() {
        let empty: Vec<TransactionHistoryEntry> = vec![];
        let entries = vec![make_tx_entry(100), make_tx_entry(101)];

        let mismatches = compare_entries(&empty, &entries);
        assert_eq!(mismatches.len(), 2);
        assert!(mismatches[0]
            .detail
            .contains("present in reference but missing in local"));
        assert!(mismatches[1]
            .detail
            .contains("present in reference but missing in local"));

        let mismatches = compare_entries(&entries, &empty);
        assert_eq!(mismatches.len(), 2);
        assert!(mismatches[0]
            .detail
            .contains("present in local but missing in reference"));
        assert!(mismatches[1]
            .detail
            .contains("present in local but missing in reference"));
    }

    #[test]
    fn test_compare_entries_duplicate_ledger_seq_local() {
        let local = vec![make_tx_entry(100), make_tx_entry(100)];
        let reference = vec![make_tx_entry(100)];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0]
            .detail
            .contains("local entries not strictly ordered"));
    }

    #[test]
    fn test_compare_entries_duplicate_ledger_seq_reference() {
        let local = vec![make_tx_entry(100)];
        let reference = vec![make_tx_entry(100), make_tx_entry(100)];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0]
            .detail
            .contains("reference entries not strictly ordered"));
    }

    #[test]
    fn test_compare_entries_out_of_order_local() {
        let local = vec![make_tx_entry(102), make_tx_entry(101)];
        let reference = vec![make_tx_entry(101), make_tx_entry(102)];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0]
            .detail
            .contains("local entries not strictly ordered"));
    }

    #[test]
    fn test_compare_entries_out_of_order_reference() {
        let local = vec![make_tx_entry(101), make_tx_entry(102)];
        let reference = vec![make_tx_entry(102), make_tx_entry(101)];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0]
            .detail
            .contains("reference entries not strictly ordered"));
    }

    // ========================================================================
    // compare_entries tests — TransactionHistoryResultEntry
    // ========================================================================

    #[test]
    fn test_compare_result_entries_identical() {
        let entries = vec![make_result_entry(100), make_result_entry(101)];
        let mismatches = compare_entries(&entries, &entries);
        assert!(mismatches.is_empty());
    }

    #[test]
    fn test_compare_result_entries_local_has_extra() {
        let local = vec![
            make_result_entry(100),
            make_result_entry(101),
            make_result_entry(102),
        ];
        let reference = vec![make_result_entry(100), make_result_entry(102)];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 1);
        assert_eq!(
            mismatches[0].detail,
            "ledger 101: tx_result_set entry present in local but missing in reference"
        );
        assert_eq!(mismatches[0].category, Category::Results);
    }

    #[test]
    fn test_compare_result_entries_payload_mismatch() {
        let local = vec![make_result_entry_with_results(100, 1)];
        let reference = vec![make_result_entry_with_results(100, 2)];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0]
            .detail
            .starts_with("ledger 100: tx_result_set differs"));
        assert_eq!(mismatches[0].category, Category::Results);
    }

    #[test]
    fn test_compare_result_entries_interleaved_and_mismatch() {
        let local = vec![
            make_result_entry(100),
            make_result_entry(102),
            make_result_entry_with_results(104, 1),
        ];
        let reference = vec![
            make_result_entry(101),
            make_result_entry(102),
            make_result_entry_with_results(104, 2),
        ];
        let mismatches = compare_entries(&local, &reference);
        assert_eq!(mismatches.len(), 3);
        assert!(mismatches[0].detail.contains("ledger 100"));
        assert!(mismatches[0]
            .detail
            .contains("present in local but missing in reference"));
        assert!(mismatches[1].detail.contains("ledger 101"));
        assert!(mismatches[1]
            .detail
            .contains("present in reference but missing in local"));
        assert!(mismatches[2]
            .detail
            .starts_with("ledger 104: tx_result_set differs"));
    }
}
