//! Header comparison utility for debugging ledger hash mismatches.
//!
//! This binary compares ledger headers between a local database and a history
//! archive, helping identify discrepancies in ledger state. This is useful for
//! debugging hash mismatches during catchup or replay testing.
//!
//! # Usage
//!
//! ```bash
//! header_compare --ledger 310000 --config testnet.toml
//! header_compare --ledger 310000 --config testnet.toml --compare-results
//! ```
//!
//! # Output
//!
//! The tool displays a side-by-side comparison of header fields including:
//! - Ledger hash
//! - Previous ledger hash
//! - Protocol version
//! - Close time
//! - Transaction set hash
//! - Bucket list hash
//! - Fee pool and total coins
//!
//! When `--compare-results` is specified, it also compares transaction result
//! sets to identify individual transaction execution differences.

use clap::Parser;
use henyey_app::config::AppConfig;
use henyey_common::Hash256;
use henyey_history::HistoryArchive;
use henyey_ledger::compute_header_hash;
use std::path::PathBuf;
use stellar_xdr::curr::{LedgerHeader, TransactionHistoryResultEntry, WriteXdr};

/// CLI arguments for the header comparison tool.
#[derive(Parser)]
#[command(about = "Compare local and archive ledger headers")]
struct Args {
    /// Ledger sequence number to compare.
    #[arg(long)]
    ledger: u32,

    /// Path to the configuration file.
    #[arg(long, default_value = "testnet-validator.toml")]
    config: PathBuf,

    /// Optional database path override (defaults to config value).
    #[arg(long)]
    db: Option<PathBuf>,

    /// Also compare transaction result sets between local DB and archive.
    #[arg(long)]
    compare_results: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = AppConfig::from_file_with_env(&args.config)?;

    let db_path = args.db.unwrap_or_else(|| config.database.path.clone());
    let db = henyey_db::Database::open(&db_path)?;
    let local_header = db
        .get_ledger_header(args.ledger)?
        .ok_or_else(|| anyhow::anyhow!("missing ledger header {} in db", args.ledger))?;
    let local_hash = compute_header_hash(&local_header)?;

    let archive = config
        .history
        .archives
        .iter()
        .find(|a| a.get_enabled)
        .ok_or_else(|| anyhow::anyhow!("no enabled history archives in config"))?;
    let archive = HistoryArchive::with_name(&archive.url, &archive.name)?;
    let checkpoint = henyey_history::checkpoint::checkpoint_containing(args.ledger);
    let headers = archive.fetch_ledger_headers(checkpoint).await?;
    let archive_header = headers
        .iter()
        .find(|entry| entry.header.ledger_seq == args.ledger)
        .map(|entry| entry.header.clone())
        .ok_or_else(|| anyhow::anyhow!("ledger header {} not found in archive", args.ledger))?;
    let archive_hash = compute_header_hash(&archive_header)?;

    println!("Ledger {}", args.ledger);
    println!();
    print_header("local", &local_header, &local_hash);
    print_header("archive", &archive_header, &archive_hash);

    if local_hash == archive_hash {
        println!();
        println!("Hashes match");
    } else {
        println!();
        println!("Hashes differ");
    }

    if args.compare_results {
        compare_tx_results(&db, &archive, args.ledger, checkpoint).await?;
    }

    Ok(())
}

/// Prints a ledger header in a human-readable format.
///
/// Displays all relevant header fields including hashes, protocol version,
/// timing, and network parameters.
fn print_header(label: &str, header: &LedgerHeader, hash: &Hash256) {
    println!("{}:", label);
    println!("  hash: {}", hash.to_hex());
    println!(
        "  prev_hash: {}",
        Hash256::from(header.previous_ledger_hash.0).to_hex()
    );
    println!("  ledger_version: {}", header.ledger_version);
    println!("  ledger_seq: {}", header.ledger_seq);
    println!("  close_time: {}", header.scp_value.close_time.0);
    println!(
        "  tx_set_hash: {}",
        Hash256::from(header.scp_value.tx_set_hash.0).to_hex()
    );
    println!(
        "  tx_result_hash: {}",
        Hash256::from(header.tx_set_result_hash.0).to_hex()
    );
    println!(
        "  bucket_list_hash: {}",
        Hash256::from(header.bucket_list_hash.0).to_hex()
    );
    println!("  total_coins: {}", header.total_coins);
    println!("  fee_pool: {}", header.fee_pool);
    println!("  inflation_seq: {}", header.inflation_seq);
    println!("  base_fee: {}", header.base_fee);
    println!("  base_reserve: {}", header.base_reserve);
    println!("  max_tx_set_size: {}", header.max_tx_set_size);
    println!("  id_pool: {}", header.id_pool);
    println!("  upgrades: {}", header.scp_value.upgrades.len());
}

/// Compares transaction results between local database and archive.
///
/// Fetches transaction results for the specified ledger from both the local
/// database and the history archive, then compares them. Handles sparse entries
/// gracefully: when a ledger has no transactions, stellar-core omits the entry
/// from checkpoint archives (CheckpointBuilder.cpp:140), and henyey's catchup
/// path mirrors this sparsity (persist.rs:33-41).
async fn compare_tx_results(
    db: &henyey_db::Database,
    archive: &HistoryArchive,
    ledger: u32,
    checkpoint: u32,
) -> anyhow::Result<()> {
    let local_entry = db.get_tx_result_entry(ledger)?;

    let archive_entries = archive.fetch_results(checkpoint).await?;
    let archive_entry = archive_entries
        .into_iter()
        .find(|entry| entry.ledger_seq == ledger);

    println!();
    println!("Tx result set:");
    match (&local_entry, &archive_entry) {
        (Some(local), Some(archive)) => {
            print_tx_result_hash("local", local);
            print_tx_result_hash("archive", archive);
        }
        (Some(local), None) => {
            print_tx_result_hash("local", local);
            println!("  archive: (sparse)");
        }
        (None, Some(archive)) => {
            println!("  local: (sparse)");
            print_tx_result_hash("archive", archive);
        }
        (None, None) => {
            println!("  local: (sparse)");
            println!("  archive: (sparse)");
        }
    }

    let diffs =
        compare_optional_result_entries(local_entry.as_ref(), archive_entry.as_ref(), ledger)?;
    for diff in &diffs {
        println!("  {diff}");
    }

    Ok(())
}

/// Compares optional tx result entries, handling sparse (None) entries gracefully.
///
/// Returns a list of human-readable difference descriptions. An empty vec means
/// the entries are equivalent (both present and identical, or both canonically absent).
fn compare_optional_result_entries(
    local: Option<&TransactionHistoryResultEntry>,
    archive: Option<&TransactionHistoryResultEntry>,
    ledger: u32,
) -> anyhow::Result<Vec<String>> {
    match (local, archive) {
        (None, None) => {
            // Both sparse — canonical empty ledger (catchup-populated DB + archive).
            Ok(vec![format!(
                "ledger {ledger}: no tx results (both sparse)"
            )])
        }
        (Some(local_entry), None) => {
            if local_entry.tx_result_set.results.is_empty() {
                // Canonical: local populated via normal-close (stores empty entry),
                // archive is sparse (omits empty ledgers).
                Ok(vec![format!(
                    "ledger {ledger}: no tx results (local entry empty, archive sparse)"
                )])
            } else {
                // Genuine mismatch: local executed transactions but archive has no entry.
                Ok(vec![format!(
                    "MISMATCH: local has {} tx result(s) but archive entry is missing for ledger {ledger}",
                    local_entry.tx_result_set.results.len()
                )])
            }
        }
        (None, Some(archive_entry)) => {
            if archive_entry.tx_result_set.results.is_empty() {
                // Non-canonical: archives omit empty entries rather than emitting them empty.
                Ok(vec![format!(
                    "ANOMALY: archive has empty tx result entry for ledger {ledger} (non-canonical; archives normally omit empty entries)"
                )])
            } else {
                // Genuine mismatch: archive has results but local is sparse.
                Ok(vec![format!(
                    "MISMATCH: archive has {} tx result(s) but local entry is missing for ledger {ledger}",
                    archive_entry.tx_result_set.results.len()
                )])
            }
        }
        (Some(local_entry), Some(archive_entry)) => {
            compare_present_entries(local_entry, archive_entry)
        }
    }
}

/// Compares two present tx result entries transaction-by-transaction.
///
/// Returns descriptions of any differences found (count mismatch, per-tx divergence).
fn compare_present_entries(
    local: &TransactionHistoryResultEntry,
    archive: &TransactionHistoryResultEntry,
) -> anyhow::Result<Vec<String>> {
    let mut diffs = Vec::new();
    let local_results = &local.tx_result_set.results;
    let archive_results = &archive.tx_result_set.results;

    if local_results.len() != archive_results.len() {
        diffs.push(format!(
            "result count differs: local={}, archive={}",
            local_results.len(),
            archive_results.len()
        ));
    }

    let count = local_results.len().min(archive_results.len());
    for i in 0..count {
        let local_bytes = local_results[i].to_xdr(stellar_xdr::curr::Limits::none())?;
        let archive_bytes = archive_results[i].to_xdr(stellar_xdr::curr::Limits::none())?;
        if local_bytes != archive_bytes {
            diffs.push(format!(
                "tx[{}] mismatch: local fee={} result={:?} | archive fee={} result={:?}",
                i,
                local_results[i].result.fee_charged,
                local_results[i].result.result,
                archive_results[i].result.fee_charged,
                archive_results[i].result.result,
            ));
        }
    }

    Ok(diffs)
}

/// Prints the hash of a transaction result set for comparison.
fn print_tx_result_hash(label: &str, entry: &TransactionHistoryResultEntry) {
    let bytes = entry
        .tx_result_set
        .to_xdr(stellar_xdr::curr::Limits::none())
        .unwrap_or_default();
    let hash = Hash256::hash(&bytes);
    println!("  {} hash: {}", label, hash.to_hex());
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        Hash, TransactionResult, TransactionResultExt, TransactionResultPair,
        TransactionResultResult, TransactionResultSet, VecM,
    };

    /// Helper: create an empty TransactionHistoryResultEntry (no transactions).
    fn empty_result_entry(ledger: u32) -> TransactionHistoryResultEntry {
        TransactionHistoryResultEntry {
            ledger_seq: ledger,
            tx_result_set: TransactionResultSet {
                results: VecM::default(),
            },
            ext: stellar_xdr::curr::TransactionHistoryResultEntryExt::V0,
        }
    }

    /// Helper: create a TransactionHistoryResultEntry with one result pair.
    fn non_empty_result_entry(ledger: u32, fee: i64) -> TransactionHistoryResultEntry {
        let pair = TransactionResultPair {
            transaction_hash: Hash([0u8; 32]),
            result: TransactionResult {
                fee_charged: fee,
                result: TransactionResultResult::TxSuccess(VecM::default()),
                ext: TransactionResultExt::V0,
            },
        };
        TransactionHistoryResultEntry {
            ledger_seq: ledger,
            tx_result_set: TransactionResultSet {
                results: vec![pair].try_into().unwrap(),
            },
            ext: stellar_xdr::curr::TransactionHistoryResultEntryExt::V0,
        }
    }

    #[test]
    fn test_both_sparse_empty_ledger() {
        let diffs = compare_optional_result_entries(None, None, 100).unwrap();
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("no tx results"));
        assert!(diffs[0].contains("both sparse"));
    }

    #[test]
    fn test_local_empty_archive_sparse() {
        let local = empty_result_entry(100);
        let diffs = compare_optional_result_entries(Some(&local), None, 100).unwrap();
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("no tx results"));
        assert!(diffs[0].contains("local entry empty"));
        assert!(diffs[0].contains("archive sparse"));
    }

    #[test]
    fn test_local_non_empty_archive_sparse_is_mismatch() {
        let local = non_empty_result_entry(100, 200);
        let diffs = compare_optional_result_entries(Some(&local), None, 100).unwrap();
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("MISMATCH"));
        assert!(diffs[0].contains("local has 1 tx result(s)"));
    }

    #[test]
    fn test_local_sparse_archive_non_empty_is_mismatch() {
        let archive = non_empty_result_entry(100, 300);
        let diffs = compare_optional_result_entries(None, Some(&archive), 100).unwrap();
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("MISMATCH"));
        assert!(diffs[0].contains("archive has 1 tx result(s)"));
    }

    #[test]
    fn test_local_sparse_archive_empty_is_anomaly() {
        let archive = empty_result_entry(100);
        let diffs = compare_optional_result_entries(None, Some(&archive), 100).unwrap();
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("ANOMALY"));
        assert!(diffs[0].contains("non-canonical"));
    }

    #[test]
    fn test_both_present_identical() {
        let local = non_empty_result_entry(100, 200);
        let archive = non_empty_result_entry(100, 200);
        let diffs = compare_optional_result_entries(Some(&local), Some(&archive), 100).unwrap();
        assert!(diffs.is_empty());
    }

    #[test]
    fn test_both_present_different_fee() {
        let local = non_empty_result_entry(100, 200);
        let archive = non_empty_result_entry(100, 300);
        let diffs = compare_optional_result_entries(Some(&local), Some(&archive), 100).unwrap();
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("tx[0] mismatch"));
        assert!(diffs[0].contains("fee=200"));
        assert!(diffs[0].contains("fee=300"));
    }

    #[test]
    fn test_both_present_different_count() {
        let local = non_empty_result_entry(100, 200);
        let archive = empty_result_entry(100);
        let diffs = compare_optional_result_entries(Some(&local), Some(&archive), 100).unwrap();
        assert!(diffs.iter().any(|d| d.contains("result count differs")));
        assert!(diffs.iter().any(|d| d.contains("local=1")));
        assert!(diffs.iter().any(|d| d.contains("archive=0")));
    }
}
