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
    compare_has(&local_has, &ref_has, &mut mismatches);

    // --- Ledger headers ---
    let local_headers = local.fetch_ledger_headers(checkpoint).await?;
    let ref_headers = reference.fetch_ledger_headers(checkpoint).await?;
    compare_ledger_headers(&local_headers, &ref_headers, &mut mismatches);

    // --- Transactions ---
    let local_txs = local.fetch_transactions(checkpoint).await?;
    let ref_txs = reference.fetch_transactions(checkpoint).await?;
    compare_entries(&local_txs, &ref_txs, &mut mismatches);

    // --- Results ---
    let local_results = local.fetch_results(checkpoint).await?;
    let ref_results = reference.fetch_results(checkpoint).await?;
    compare_entries(&local_results, &ref_results, &mut mismatches);

    Ok(CheckpointComparison {
        checkpoint,
        mismatches,
    })
}

// ============================================================================
// HAS comparison
// ============================================================================

fn compare_has(
    local: &HistoryArchiveState,
    reference: &HistoryArchiveState,
    out: &mut Vec<Mismatch>,
) {
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
        return;
    }

    for (i, (l, r)) in local_levels.iter().zip(ref_levels.iter()).enumerate() {
        if l.curr != r.curr {
            out.push(Mismatch {
                category: Category::Has,
                detail: format!(
                    "bucket level {} curr: local={} reference={}",
                    i,
                    &l.curr[..16.min(l.curr.len())],
                    &r.curr[..16.min(r.curr.len())],
                ),
            });
        }
        if l.snap != r.snap {
            out.push(Mismatch {
                category: Category::Has,
                detail: format!(
                    "bucket level {} snap: local={} reference={}",
                    i,
                    &l.snap[..16.min(l.snap.len())],
                    &r.snap[..16.min(r.snap.len())],
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
                return;
            }
            for (i, (l, r)) in l_hot.iter().zip(r_hot.iter()).enumerate() {
                if l.curr != r.curr {
                    out.push(Mismatch {
                        category: Category::Has,
                        detail: format!(
                            "hot archive bucket level {} curr: local={} reference={}",
                            i,
                            &l.curr[..16.min(l.curr.len())],
                            &r.curr[..16.min(r.curr.len())],
                        ),
                    });
                }
                if l.snap != r.snap {
                    out.push(Mismatch {
                        category: Category::Has,
                        detail: format!(
                            "hot archive bucket level {} snap: local={} reference={}",
                            i,
                            &l.snap[..16.min(l.snap.len())],
                            &r.snap[..16.min(r.snap.len())],
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
}

// ============================================================================
// Ledger header comparison
// ============================================================================

fn compare_ledger_headers(
    local: &[LedgerHeaderHistoryEntry],
    reference: &[LedgerHeaderHistoryEntry],
    out: &mut Vec<Mismatch>,
) {
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

fn compare_entries<T: ComparableEntry>(local: &[T], reference: &[T], out: &mut Vec<Mismatch>) {
    let category = T::category();
    if local.len() != reference.len() {
        out.push(Mismatch {
            category,
            detail: format!(
                "entry count: local={} reference={}",
                local.len(),
                reference.len()
            ),
        });
    }

    let min_len = local.len().min(reference.len());
    for i in 0..min_len {
        let l = &local[i];
        let r = &reference[i];

        if l.ledger_seq() != r.ledger_seq() {
            out.push(Mismatch {
                category,
                detail: format!(
                    "entry {}: ledger_seq local={} reference={}",
                    i,
                    l.ledger_seq(),
                    r.ledger_seq()
                ),
            });
            continue;
        }

        let l_xdr = l.payload_xdr();
        let r_xdr = r.payload_xdr();
        match (l_xdr, r_xdr) {
            (Ok(l_bytes), Ok(r_bytes)) if l_bytes != r_bytes => {
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
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_state::{HASBucketLevel, HASBucketNext};

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
            current_ledger: ledger.into(),
            network_passphrase: None,
            current_buckets: levels,
            hot_archive_buckets: None,
        }
    }

    #[test]
    fn test_has_match() {
        let has = make_has(63, &["aaaa", "bbbb"]);
        let mut mismatches = Vec::new();
        compare_has(&has, &has.clone(), &mut mismatches);
        assert!(mismatches.is_empty());
    }

    #[test]
    fn test_has_ledger_mismatch() {
        let a = make_has(63, &["aaaa"]);
        let b = make_has(127, &["aaaa"]);
        let mut mismatches = Vec::new();
        compare_has(&a, &b, &mut mismatches);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0].detail.contains("currentLedger"));
    }

    #[test]
    fn test_has_bucket_mismatch() {
        let a = make_has(63, &["aaaa"]);
        let b = make_has(63, &["bbbb"]);
        let mut mismatches = Vec::new();
        compare_has(&a, &b, &mut mismatches);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0].detail.contains("curr"));
    }

    #[test]
    fn test_has_level_count_mismatch() {
        let a = make_has(63, &["aaaa", "bbbb"]);
        let b = make_has(63, &["aaaa"]);
        let mut mismatches = Vec::new();
        compare_has(&a, &b, &mut mismatches);
        assert_eq!(mismatches.len(), 1);
        assert!(mismatches[0].detail.contains("bucket level count"));
    }
}
