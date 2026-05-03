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
/// database and the history archive, then compares them transaction by
/// transaction. Any differences (in fee charged or result code) are printed.
///
/// This helps identify which specific transactions produced different results
/// when debugging execution divergence.
async fn compare_tx_results(
    db: &henyey_db::Database,
    archive: &HistoryArchive,
    ledger: u32,
    checkpoint: u32,
) -> anyhow::Result<()> {
    let local_entry = db
        .get_tx_result_entry(ledger)?
        .ok_or_else(|| anyhow::anyhow!("missing tx result entry {} in db", ledger))?;

    let archive_entries = archive.fetch_results(checkpoint).await?;
    let archive_entry = archive_entries
        .into_iter()
        .find(|entry| entry.ledger_seq == ledger)
        .ok_or_else(|| anyhow::anyhow!("tx result entry {} not found in archive", ledger))?;

    println!();
    println!("Tx result set:");
    print_tx_result_hash("local", &local_entry);
    print_tx_result_hash("archive", &archive_entry);

    let local_results = &local_entry.tx_result_set.results;
    let archive_results = &archive_entry.tx_result_set.results;

    if local_results.len() != archive_results.len() {
        println!(
            "  result count differs: local={}, archive={}",
            local_results.len(),
            archive_results.len()
        );
    }

    let count = local_results.len().min(archive_results.len());
    for i in 0..count {
        let local_pair = &local_results[i];
        let archive_pair = &archive_results[i];

        let local_bytes = local_pair.to_xdr(stellar_xdr::curr::Limits::none())?;
        let archive_bytes = archive_pair.to_xdr(stellar_xdr::curr::Limits::none())?;
        if local_bytes != archive_bytes {
            let local_summary = format!("{:?}", local_pair.result.result);
            let archive_summary = format!("{:?}", archive_pair.result.result);
            println!(
                "  tx[{}] mismatch: local fee={} result={} | archive fee={} result={}",
                i,
                local_pair.result.fee_charged,
                local_summary,
                archive_pair.result.fee_charged,
                archive_summary,
            );
        }
    }

    Ok(())
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
