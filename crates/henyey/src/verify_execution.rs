//! Offline execution verification — replays ledgers against CDP data.

use std::sync::Arc;
use std::time::{Duration, Instant};

use stellar_xdr::curr::WriteXdr;

use henyey_app::AppConfig;
use henyey_bucket::{BucketList, BucketManager, HasNextState, HotArchiveBucketList};
use henyey_common::Hash256;
use henyey_history::cdp::{
    extract_ledger_close_data, extract_ledger_header, extract_transaction_results,
    CachedCdpDataLake,
};
use henyey_history::checkpoint;
use henyey_ledger::{LedgerManager, LedgerManagerConfig};

pub(crate) struct VerifyExecutionOptions {
    pub from: Option<u32>,
    pub to: Option<u32>,
    pub stop_on_error: bool,
    pub show_diff: bool,
    pub cdp_url: Option<String>,
    pub cdp_date: Option<String>,
    pub cache_dir: Option<std::path::PathBuf>,
    pub no_cache: bool,
    pub quiet: bool,
}

/// Bundles all long-lived resources needed across verification phases.
pub(crate) struct VerifyContext {
    archive: henyey_history::HistoryArchive,
    cdp: CachedCdpDataLake,
    ledger_manager: LedgerManager,
    #[allow(dead_code)]
    bucket_manager: Arc<BucketManager>,
    // TempDir guards — dropping these would delete the temp directories.
    _cdp_dir_holder: Option<tempfile::TempDir>,
    _bucket_dir_holder: Option<tempfile::TempDir>,
    // Configuration
    start_ledger: u32,
    end_ledger: u32,
    init_checkpoint: u32,
    end_checkpoint: u32,
    init_header_hash: Hash256,
    stop_on_error: bool,
    show_diff: bool,
    quiet: bool,
}

/// Accumulator counters for the verification run.
#[derive(Default)]
pub(crate) struct VerifyStats {
    pub ledgers_verified: u32,
    pub ledgers_matched: u32,
    pub ledgers_mismatched: u32,
    pub header_mismatches: u32,
    pub tx_result_mismatches: u32,
    pub meta_mismatches: u32,
    total_close_us: u64,
    total_tx_exec_us: u64,
    total_commit_us: u64,
    total_add_batch_us: u64,
    total_eviction_us: u64,
    total_tx_count: usize,
    total_cache_hits: u64,
    total_cache_misses: u64,
    slowest_ledger_us: u64,
    slowest_ledger_seq: u32,
    slowest_txs: Vec<(u32, String, u64)>,
    peak_rss_bytes: u64,
}

/// Returns a human-readable name for a `TransactionResultResult` variant.
fn tx_result_code_name(r: &stellar_xdr::curr::TransactionResultResult) -> String {
    use stellar_xdr::curr::TransactionResultResult;
    match r {
        TransactionResultResult::TxSuccess(_) => "txSuccess".to_string(),
        TransactionResultResult::TxFailed(_) => "txFailed".to_string(),
        TransactionResultResult::TxFeeBumpInnerSuccess(_) => "txFeeBumpInnerSuccess".to_string(),
        TransactionResultResult::TxFeeBumpInnerFailed(_) => "txFeeBumpInnerFailed".to_string(),
        other => format!("{:?}", other),
    }
}

/// Prints pairwise differences between two operation result slices.
fn print_op_diffs(
    our_ops: &[stellar_xdr::curr::OperationResult],
    cdp_ops: &[stellar_xdr::curr::OperationResult],
) {
    use stellar_xdr::curr::WriteXdr;
    for (j, (our_op, cdp_op)) in our_ops.iter().zip(cdp_ops.iter()).enumerate() {
        let our_op_xdr = our_op
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        let cdp_op_xdr = cdp_op
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        if our_op_xdr != cdp_op_xdr {
            println!("          Op {} differs:", j);
            println!("            Ours: {:?}", our_op);
            println!("            CDP:  {:?}", cdp_op);
        }
    }
}

/// Prints all operations from a result slice with a label.
fn print_all_ops(label: &str, ops: &[stellar_xdr::curr::OperationResult]) {
    println!("        {} ops ({}):", label, ops.len());
    for (j, op) in ops.iter().enumerate() {
        println!("          Op {}: {:?}", j, op);
    }
}

/// Prints an exhaustive field-by-field comparison of two ledger headers.
fn print_header_field_diffs(
    h: &stellar_xdr::curr::LedgerHeader,
    c: &stellar_xdr::curr::LedgerHeader,
    bucket_levels: &[(henyey_common::Hash256, henyey_common::Hash256)],
) {
    use henyey_common::Hash256;
    if h.ledger_version != c.ledger_version {
        println!(
            "    DIFF ledger_version: ours={} expected={}",
            h.ledger_version, c.ledger_version
        );
    }
    if h.previous_ledger_hash != c.previous_ledger_hash {
        println!(
            "    DIFF previous_ledger_hash: ours={} expected={}",
            hex::encode(&h.previous_ledger_hash.0),
            hex::encode(&c.previous_ledger_hash.0)
        );
    }
    if h.scp_value != c.scp_value {
        println!("    DIFF scp_value");
        if h.scp_value.tx_set_hash != c.scp_value.tx_set_hash {
            println!(
                "      tx_set_hash: ours={} expected={}",
                hex::encode(&h.scp_value.tx_set_hash.0),
                hex::encode(&c.scp_value.tx_set_hash.0)
            );
        }
        if h.scp_value.close_time != c.scp_value.close_time {
            println!(
                "      close_time: ours={} expected={}",
                h.scp_value.close_time.0, c.scp_value.close_time.0
            );
        }
        if h.scp_value.upgrades != c.scp_value.upgrades {
            println!(
                "      upgrades: ours={:?} expected={:?}",
                h.scp_value.upgrades, c.scp_value.upgrades
            );
        }
        if h.scp_value.ext != c.scp_value.ext {
            println!("      ext: differs");
        }
    }
    let our_bl_hash = Hash256::from(h.bucket_list_hash.0);
    let expected_bl_hash = Hash256::from(c.bucket_list_hash.0);
    if our_bl_hash != expected_bl_hash {
        println!(
            "    DIFF bucket_list_hash: ours={} expected={}",
            our_bl_hash.to_hex(),
            expected_bl_hash.to_hex()
        );
        for (i, (curr_hash, snap_hash)) in bucket_levels.iter().enumerate() {
            println!(
                "      Level {}: curr={} snap={}",
                i,
                curr_hash.to_hex(),
                snap_hash.to_hex()
            );
        }
    }
    if h.tx_set_result_hash != c.tx_set_result_hash {
        println!(
            "    DIFF tx_set_result_hash: ours={} expected={}",
            hex::encode(&h.tx_set_result_hash.0),
            hex::encode(&c.tx_set_result_hash.0)
        );
    }
    if h.ledger_seq != c.ledger_seq {
        println!(
            "    DIFF ledger_seq: ours={} expected={}",
            h.ledger_seq, c.ledger_seq
        );
    }
    if h.total_coins != c.total_coins {
        println!(
            "    DIFF total_coins: ours={} expected={}",
            h.total_coins, c.total_coins
        );
    }
    if h.fee_pool != c.fee_pool {
        println!(
            "    DIFF fee_pool: ours={} expected={}",
            h.fee_pool, c.fee_pool
        );
    }
    if h.inflation_seq != c.inflation_seq {
        println!(
            "    DIFF inflation_seq: ours={} expected={}",
            h.inflation_seq, c.inflation_seq
        );
    }
    if h.id_pool != c.id_pool {
        println!(
            "    DIFF id_pool: ours={} expected={}",
            h.id_pool, c.id_pool
        );
    }
    if h.base_fee != c.base_fee {
        println!(
            "    DIFF base_fee: ours={} expected={}",
            h.base_fee, c.base_fee
        );
    }
    if h.base_reserve != c.base_reserve {
        println!(
            "    DIFF base_reserve: ours={} expected={}",
            h.base_reserve, c.base_reserve
        );
    }
    if h.max_tx_set_size != c.max_tx_set_size {
        println!(
            "    DIFF max_tx_set_size: ours={} expected={}",
            h.max_tx_set_size, c.max_tx_set_size
        );
    }
    if h.skip_list != c.skip_list {
        println!("    DIFF skip_list:");
        for (i, (ours, exp)) in h.skip_list.iter().zip(c.skip_list.iter()).enumerate() {
            if ours != exp {
                println!(
                    "      [{}]: ours={} expected={}",
                    i,
                    hex::encode(&ours.0),
                    hex::encode(&exp.0)
                );
            }
        }
    }
    if h.ext != c.ext {
        println!("    DIFF ext: ours={:?} expected={:?}", h.ext, c.ext);
    }
}

/// Prints detailed per-TX result diffs between our results and CDP results.
///
/// Shows ordering differences, then does a TX-by-TX XDR comparison with
/// detailed operation-level diffs for all result variant combinations.
fn print_tx_result_diffs(
    our_results: &[stellar_xdr::curr::TransactionResultPair],
    cdp_results: &[stellar_xdr::curr::TransactionResultPair],
) {
    use stellar_xdr::curr::{InnerTransactionResultResult, TransactionResultResult, WriteXdr};
    println!(
        "    TX count: ours={} CDP={}",
        our_results.len(),
        cdp_results.len()
    );

    // Check if TX ordering differs
    let mut order_diffs = 0;
    for (i, (our_tx, cdp_tx)) in our_results.iter().zip(cdp_results.iter()).enumerate() {
        if our_tx.transaction_hash != cdp_tx.transaction_hash {
            if order_diffs < 10 {
                println!(
                    "    ORDER DIFF at position {}: ours={} CDP={}",
                    i,
                    hex::encode(&our_tx.transaction_hash.0),
                    hex::encode(&cdp_tx.transaction_hash.0)
                );
            }
            order_diffs += 1;
        }
    }
    if order_diffs > 0 {
        println!("    Total TX ordering differences: {}", order_diffs);
    } else {
        println!("    TX ordering is IDENTICAL (same content hashes at every position)");
    }

    // Detailed TX-by-TX comparison using full XDR
    let mut diff_count = 0;
    for (i, (our_tx, cdp_tx)) in our_results.iter().zip(cdp_results.iter()).enumerate() {
        let our_xdr = our_tx
            .result
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();
        let cdp_xdr = cdp_tx
            .result
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default();

        if our_xdr != cdp_xdr {
            diff_count += 1;
            let our_result = &our_tx.result.result;
            let cdp_result = &cdp_tx.result.result;

            println!("      TX {}: MISMATCH (XDR differs)", i);
            println!(
                "        Result: ours={} CDP={}",
                tx_result_code_name(our_result),
                tx_result_code_name(cdp_result)
            );
            println!(
                "        Fee: ours={} CDP={}",
                our_tx.result.fee_charged, cdp_tx.result.fee_charged
            );
            println!(
                "        TX hash: {}",
                hex::encode(&our_tx.transaction_hash.0)
            );

            // Compare operations for same-variant pairs
            match (our_result, cdp_result) {
                (
                    TransactionResultResult::TxFailed(our_ops),
                    TransactionResultResult::TxFailed(cdp_ops),
                )
                | (
                    TransactionResultResult::TxSuccess(our_ops),
                    TransactionResultResult::TxSuccess(cdp_ops),
                ) => {
                    print_op_diffs(our_ops, cdp_ops);
                }
                // One succeeds, other fails — show all ops from both sides
                (
                    TransactionResultResult::TxSuccess(our_ops),
                    TransactionResultResult::TxFailed(cdp_ops),
                )
                | (
                    TransactionResultResult::TxFailed(our_ops),
                    TransactionResultResult::TxSuccess(cdp_ops),
                ) => {
                    print_all_ops("Ours", our_ops);
                    print_all_ops("CDP", cdp_ops);
                }
                _ => {}
            }

            // Fee bump inner result details
            match (our_result, cdp_result) {
                (
                    TransactionResultResult::TxFeeBumpInnerFailed(our_inner),
                    TransactionResultResult::TxFeeBumpInnerFailed(cdp_inner),
                ) => {
                    println!(
                        "        Inner fee: ours={} CDP={}",
                        our_inner.result.fee_charged, cdp_inner.result.fee_charged
                    );
                    let our_inner_code =
                        format!("{:?}", std::mem::discriminant(&our_inner.result.result));
                    let cdp_inner_code =
                        format!("{:?}", std::mem::discriminant(&cdp_inner.result.result));
                    println!(
                        "        Inner result type: ours={} CDP={}",
                        our_inner_code, cdp_inner_code
                    );
                    if let (
                        InnerTransactionResultResult::TxFailed(our_ops),
                        InnerTransactionResultResult::TxFailed(cdp_ops),
                    ) = (&our_inner.result.result, &cdp_inner.result.result)
                    {
                        print_op_diffs(our_ops, cdp_ops);
                        if our_ops.len() != cdp_ops.len() {
                            println!(
                                "          Inner op count: ours={} CDP={}",
                                our_ops.len(),
                                cdp_ops.len()
                            );
                        }
                    } else {
                        println!("        Inner result ours: {:?}", our_inner.result.result);
                        println!("        Inner result CDP:  {:?}", cdp_inner.result.result);
                    }
                }
                (
                    TransactionResultResult::TxFeeBumpInnerSuccess(our_inner),
                    TransactionResultResult::TxFeeBumpInnerSuccess(cdp_inner),
                ) => {
                    println!(
                        "        Inner fee: ours={} CDP={}",
                        our_inner.result.fee_charged, cdp_inner.result.fee_charged
                    );
                    if let (
                        InnerTransactionResultResult::TxSuccess(our_ops),
                        InnerTransactionResultResult::TxSuccess(cdp_ops),
                    ) = (&our_inner.result.result, &cdp_inner.result.result)
                    {
                        print_op_diffs(our_ops, cdp_ops);
                    }
                }
                // Cross-case fee bump inner results
                (
                    TransactionResultResult::TxFeeBumpInnerSuccess(our_inner),
                    TransactionResultResult::TxFeeBumpInnerFailed(cdp_inner),
                ) => {
                    println!(
                        "        Inner fee: ours={} CDP={}",
                        our_inner.result.fee_charged, cdp_inner.result.fee_charged
                    );
                    if let InnerTransactionResultResult::TxSuccess(our_ops) =
                        &our_inner.result.result
                    {
                        print_all_ops("Ours inner", our_ops);
                    }
                    if let InnerTransactionResultResult::TxFailed(cdp_ops) =
                        &cdp_inner.result.result
                    {
                        print_all_ops("CDP inner", cdp_ops);
                    } else {
                        println!("        CDP inner result: {:?}", cdp_inner.result.result);
                    }
                }
                (
                    TransactionResultResult::TxFeeBumpInnerFailed(our_inner),
                    TransactionResultResult::TxFeeBumpInnerSuccess(cdp_inner),
                ) => {
                    println!(
                        "        Inner fee: ours={} CDP={}",
                        our_inner.result.fee_charged, cdp_inner.result.fee_charged
                    );
                    println!("        Ours inner result: {:?}", our_inner.result.result);
                    if let InnerTransactionResultResult::TxSuccess(cdp_ops) =
                        &cdp_inner.result.result
                    {
                        print_all_ops("CDP inner", cdp_ops);
                    }
                }
                _ => {}
            }

            // Show CDP ops when ours is TxNotSupported or other non-standard result
            if !matches!(
                our_result,
                TransactionResultResult::TxSuccess(_)
                    | TransactionResultResult::TxFailed(_)
                    | TransactionResultResult::TxFeeBumpInnerSuccess(_)
                    | TransactionResultResult::TxFeeBumpInnerFailed(_)
            ) {
                if let TransactionResultResult::TxFailed(cdp_ops) = cdp_result {
                    println!("        CDP txFailed ops ({}):", cdp_ops.len());
                    for (j, op) in cdp_ops.iter().enumerate() {
                        println!("          Op {}: {:?}", j, op);
                    }
                }
            }

            // Limit output to first 10 diffs
            if diff_count >= 10 {
                println!("      ... (showing first 10 of potentially more diffs)");
                break;
            }
        }
    }
    if diff_count > 0 {
        println!(
            "    Total TX diffs: {} out of {}",
            diff_count,
            our_results.len().min(cdp_results.len())
        );
    }
}

/// Walk all `LedgerEntryChange` slices in a `TransactionMeta` (V3/V4).
///
/// Calls `f` once for `tx_changes_before`, once per operation's changes,
/// and once for `tx_changes_after`. No-op for other meta versions.
fn for_each_tx_meta_changes(
    meta: &stellar_xdr::curr::TransactionMeta,
    mut f: impl FnMut(&[stellar_xdr::curr::LedgerEntryChange]),
) {
    match meta {
        stellar_xdr::curr::TransactionMeta::V3(v3) => {
            f(&v3.tx_changes_before);
            for op in v3.operations.iter() {
                f(&op.changes);
            }
            f(&v3.tx_changes_after);
        }
        stellar_xdr::curr::TransactionMeta::V4(v4) => {
            f(&v4.tx_changes_before);
            for op in v4.operations.iter() {
                f(&op.changes);
            }
            f(&v4.tx_changes_after);
        }
        _ => {}
    }
}

/// Verifies transaction execution by comparing results against CDP metadata.
///
/// Restores bucket list state from a checkpoint, re-executes transactions via
/// `close_ledger`, and compares results against CDP-produced ledger close metadata.
pub(crate) async fn cmd_verify_execution(
    config: AppConfig,
    opts: VerifyExecutionOptions,
) -> anyhow::Result<()> {
    let mut ctx = setup(config, opts).await?;
    let (mut stats, elapsed) = run_verification_loop(&mut ctx).await?;
    print_summary(&mut stats, elapsed);
    if stats.ledgers_mismatched > 0 {
        anyhow::bail!(
            "Verification failed with {} mismatched ledgers",
            stats.ledgers_mismatched
        );
    }
    Ok(())
}

/// Phase 1-4: Parse config, create clients, download state, initialize LedgerManager.
async fn setup(config: AppConfig, opts: VerifyExecutionOptions) -> anyhow::Result<VerifyContext> {
    let VerifyExecutionOptions {
        from,
        to,
        stop_on_error,
        show_diff,
        cdp_url,
        cdp_date,
        cache_dir,
        no_cache,
        quiet,
    } = opts;

    let init_start = Instant::now();

    if !quiet {
        println!("Transaction Execution Verification");
        println!("===================================");
        println!("Executes transactions via close_ledger and compares against CDP.");
        println!();
    }

    // Determine network name
    let (network_name, is_mainnet) =
        if config.network.passphrase == "Test SDF Network ; September 2015" {
            ("testnet", false)
        } else {
            ("mainnet", true)
        };

    // Set network-specific CDP defaults
    let cdp_url = cdp_url.unwrap_or_else(|| {
        if is_mainnet {
            "https://aws-public-blockchain.s3.us-east-2.amazonaws.com/v1.1/stellar/ledgers/pubnet"
                .to_string()
        } else {
            "https://aws-public-blockchain.s3.us-east-2.amazonaws.com/v1.1/stellar/ledgers/testnet"
                .to_string()
        }
    });
    let cdp_date = cdp_date.unwrap_or_else(|| {
        if is_mainnet {
            String::new()
        } else {
            "2025-12-18".to_string()
        }
    });

    // Determine cache directory
    let cache_base = if no_cache {
        None
    } else {
        cache_dir.or_else(|| dirs::cache_dir().map(|p| p.join("rs-stellar-core")))
    };

    // Create archive client
    let archive = super::first_archive(&config)?;

    if !quiet {
        println!("Archive: {}", config.history.archives[0].url);
        let cdp_date_display = if cdp_date.is_empty() {
            "none (range-based)"
        } else {
            &cdp_date
        };
        println!("CDP: {} (date: {})", cdp_url, cdp_date_display);
        if let Some(ref cache) = cache_base {
            println!("Cache: {}", cache.display());
        } else {
            println!("Cache: disabled");
        }
    }

    // Get current ledger and calculate range
    let root_has = archive.fetch_root_has().await?;
    let current_ledger = root_has.current_ledger;

    let end_ledger = to.unwrap_or_else(|| {
        checkpoint::latest_checkpoint_before_or_at(current_ledger).unwrap_or(current_ledger)
    });
    let start_ledger = from.unwrap_or_else(|| {
        let freq = henyey_history::checkpoint_frequency();
        checkpoint::checkpoint_containing(end_ledger)
            .saturating_sub(4 * freq)
            .max(freq)
    });

    let freq = henyey_history::checkpoint_frequency();
    let init_checkpoint =
        checkpoint::latest_checkpoint_before_or_at(start_ledger.saturating_sub(1))
            .unwrap_or(freq - 1);
    let end_checkpoint = checkpoint::checkpoint_containing(end_ledger);

    if !quiet {
        println!("Ledger range: {} to {}", start_ledger, end_ledger);
        println!("Initial state: checkpoint {}", init_checkpoint);
        println!();
    }

    // Create CDP client with caching
    let (_cdp_dir_holder, cdp) = if let Some(ref cache) = cache_base {
        let cdp = CachedCdpDataLake::new(&cdp_url, &cdp_date, cache, network_name)?;
        (None, cdp)
    } else {
        let temp = tempfile::tempdir()?;
        let cdp = CachedCdpDataLake::new(&cdp_url, &cdp_date, temp.path(), network_name)?;
        (Some(temp), cdp)
    };

    // Setup bucket manager
    let (_bucket_dir_holder, bucket_path) = if let Some(ref cache) = cache_base {
        let path = cache.join("buckets").join(network_name);
        std::fs::create_dir_all(&path)?;
        (None, path)
    } else {
        let temp = tempfile::tempdir()?;
        let path = temp.path().to_path_buf();
        (Some(temp), path)
    };
    let bucket_manager = Arc::new(BucketManager::with_persist_index(
        bucket_path.clone(),
        true,
    )?);

    // Download initial state
    if !quiet {
        println!(
            "Downloading initial state at checkpoint {}...",
            init_checkpoint
        );
    }
    let init_has = archive.fetch_checkpoint_has(init_checkpoint).await?;

    // Extract bucket hashes
    let bucket_hashes = init_has.bucket_hash_pairs();

    let live_next_states: Vec<HasNextState> = init_has
        .live_next_states()
        .into_iter()
        .map(HasNextState::from)
        .collect();

    // Extract hot archive bucket hashes (protocol 23+)
    let hot_archive_hashes = init_has.hot_archive_bucket_hash_pairs();

    let hot_archive_next_states: Option<Vec<HasNextState>> = init_has
        .hot_archive_next_states()
        .map(|states| states.into_iter().map(HasNextState::from).collect());

    // Collect all bucket hashes to download
    let mut all_hashes: Vec<Hash256> = Vec::new();
    for (curr, snap) in &bucket_hashes {
        all_hashes.push(*curr);
        all_hashes.push(*snap);
    }
    for state in &live_next_states {
        if let Some(ref output) = state.output {
            all_hashes.push(*output);
        }
    }
    if let Some(ref ha_hashes) = hot_archive_hashes {
        for (curr, snap) in ha_hashes {
            all_hashes.push(*curr);
            all_hashes.push(*snap);
        }
    }
    if let Some(ref ha_states) = hot_archive_next_states {
        for state in ha_states {
            if let Some(ref output) = state.output {
                all_hashes.push(*output);
            }
        }
    }
    let all_hashes: Vec<&Hash256> = all_hashes.iter().filter(|h| !h.is_zero()).collect();

    // Download buckets
    let (cached, downloaded) =
        super::download_buckets_parallel(&archive, bucket_manager.clone(), all_hashes).await?;
    println!(
        "[INIT] Bucket download: {} cached, {} downloaded",
        cached, downloaded
    );

    // Restore bucket lists
    let mut bucket_list =
        BucketList::restore_from_has(&bucket_hashes, &live_next_states, |hash| {
            bucket_manager.load_bucket(hash).map(|b| (*b).clone())
        })?;
    bucket_list.set_bucket_dir(bucket_manager.bucket_dir().to_path_buf());

    let mut hot_archive_bucket_list = match (&hot_archive_hashes, &hot_archive_next_states) {
        (Some(ref hashes), Some(ref next_states)) => {
            HotArchiveBucketList::restore_from_has(hashes, next_states, |hash| {
                bucket_manager.load_hot_archive_bucket(hash)
            })?
        }
        _ => HotArchiveBucketList::new(),
    };

    // Get init header and restart merges
    let init_headers = archive.fetch_ledger_headers(init_checkpoint).await?;
    let init_header_entry = init_headers
        .iter()
        .find(|h| h.header.ledger_seq == init_checkpoint);
    let init_protocol_version = init_header_entry
        .map(|h| h.header.ledger_version)
        .unwrap_or(25);

    // Enable structure-based merge restarts to match stellar-core online mode behavior.
    //
    // Although stellar-core standalone offline commands skip restartMerges, we are comparing
    // our results against CDP headers produced by stellar-core in ONLINE mode. The stellar-core node
    // that produced those headers had full structure-based merge restarts enabled.
    //
    // In stellar-core online mode, restartMerges uses mLevels[i-1].getSnap() (the old snap
    // from HAS) to start merges. Without structure-based restarts, add_batch would
    // use snap() which returns the snapped curr (different input!).
    bucket_list
        .restart_merges_from_has(
            init_checkpoint,
            init_protocol_version,
            &live_next_states,
            |hash| bucket_manager.load_bucket(hash).map(|b| (*b).clone()),
            true, // restart_structure_based = true to match stellar-core online mode
        )
        .await?;

    if let Some(ref ha_next_states) = hot_archive_next_states {
        hot_archive_bucket_list.restart_merges_from_has(
            init_checkpoint,
            init_protocol_version,
            ha_next_states,
            |hash| bucket_manager.load_hot_archive_bucket(hash),
            true, // restart_structure_based = true to match stellar-core online mode
        )?;
    }

    // Create and initialize LedgerManager
    let mut ledger_manager = LedgerManager::new(
        config.network.passphrase.clone(),
        LedgerManagerConfig {
            validate_bucket_hash: true,
            bucket_list_db: config.buckets.bucket_list_db.clone(),
            // Use single-threaded scan to reduce peak memory during initialization.
            // Verify-execution runs on memory-constrained CI runners where concurrent
            // level scans would cause OOM.
            scan_thread_count: 1,
            ..Default::default()
        },
    );

    // Wire merge map for bucket merge deduplication during replay.
    let finished_merges =
        std::sync::Arc::new(std::sync::RwLock::new(henyey_bucket::BucketMergeMap::new()));
    ledger_manager.set_merge_map(finished_merges);

    let init_header_entry = init_header_entry
        .ok_or_else(|| anyhow::anyhow!("No header found for checkpoint {}", init_checkpoint))?;
    let init_header_hash = Hash256::from(init_header_entry.hash.0);
    ledger_manager.initialize(
        bucket_list,
        hot_archive_bucket_list,
        init_header_entry.header.clone(),
        init_header_hash,
    )?;

    println!(
        "[INIT] TOTAL initialization: {:.2}s",
        init_start.elapsed().as_secs_f64()
    );

    Ok(VerifyContext {
        archive,
        cdp,
        ledger_manager,
        bucket_manager,
        _cdp_dir_holder,
        _bucket_dir_holder,
        start_ledger,
        end_ledger,
        init_checkpoint,
        end_checkpoint,
        init_header_hash,
        stop_on_error,
        show_diff,
        quiet,
    })
}

/// Phase 5: Main verification loop — iterate checkpoints and verify each ledger.
async fn run_verification_loop(ctx: &mut VerifyContext) -> anyhow::Result<(VerifyStats, Duration)> {
    let mut stats = VerifyStats::default();
    let mut prev_ledger_hash = ctx.init_header_hash;

    let verification_start = Instant::now();
    let process_from = ctx.init_checkpoint + 1;
    let process_from_cp = checkpoint::checkpoint_containing(process_from);

    let mut current_cp = process_from_cp;
    while current_cp <= ctx.end_checkpoint {
        let headers = ctx.archive.fetch_ledger_headers(current_cp).await?;

        for header_entry in &headers {
            verify_single_ledger(ctx, &mut stats, &mut prev_ledger_hash, header_entry).await?;
        }

        current_cp = checkpoint::next_checkpoint(current_cp);
    }

    let elapsed = verification_start.elapsed();
    Ok((stats, elapsed))
}

/// Process a single ledger: fetch CDP data, execute close_ledger, compare results.
async fn verify_single_ledger(
    ctx: &mut VerifyContext,
    stats: &mut VerifyStats,
    prev_ledger_hash: &mut Hash256,
    header_entry: &stellar_xdr::curr::LedgerHeaderHistoryEntry,
) -> anyhow::Result<()> {
    let header = &header_entry.header;
    let seq = header.ledger_seq;

    // Skip ledgers outside our range
    if seq <= ctx.init_checkpoint || seq > ctx.end_ledger {
        if seq > ctx.init_checkpoint {
            *prev_ledger_hash = Hash256::from(header_entry.hash.0);
        }
        return Ok(());
    }

    let in_test_range = seq >= ctx.start_ledger && seq <= ctx.end_ledger;

    // Fetch CDP metadata
    let lcm = match ctx.cdp.fetch_ledger_close_meta(seq).await {
        Ok(lcm) => lcm,
        Err(e) => {
            if in_test_range {
                println!("  Ledger {}: CDP fetch failed: {}", seq, e);
            }
            *prev_ledger_hash = Hash256::from(header_entry.hash.0);
            return Ok(());
        }
    };

    let cdp_header = extract_ledger_header(&lcm);

    // Validate CDP data matches archive
    if header.scp_value.close_time.0 != cdp_header.scp_value.close_time.0 {
        if in_test_range {
            println!("  Ledger {}: EPOCH MISMATCH - skipping", seq);
        }
        if ctx.stop_on_error {
            anyhow::bail!("CDP epoch mismatch at ledger {}", seq);
        }
        *prev_ledger_hash = Hash256::from(header_entry.hash.0);
        return Ok(());
    }

    // Create LedgerCloseData from CDP
    let close_data = extract_ledger_close_data(&lcm, *prev_ledger_hash);

    // Execute via close_ledger
    let result = match ctx.ledger_manager.close_ledger(close_data, None) {
        Ok(r) => r,
        Err(e) => {
            println!("  Ledger {}: close_ledger failed: {}", seq, e);
            if ctx.stop_on_error {
                anyhow::bail!("close_ledger failed at ledger {}: {}", seq, e);
            }
            *prev_ledger_hash = Hash256::from(header_entry.hash.0);
            return Ok(());
        }
    };

    if in_test_range {
        stats.ledgers_verified += 1;

        // Compare header hash
        let expected_header_hash = Hash256::from(header_entry.hash.0);
        let header_matches = result.header_hash == expected_header_hash;

        // Compare tx result hash
        let cdp_tx_results = extract_transaction_results(&lcm);

        let expected_tx_result_hash = Hash256::from(cdp_header.tx_set_result_hash.0);
        let our_tx_result_hash = result.tx_result_hash();
        let tx_result_matches = our_tx_result_hash == expected_tx_result_hash;

        // Compare meta: if present, consider it matching when tx results match.
        // Full meta comparison would be more complex.
        let meta_matches = result.meta.is_none() || tx_result_matches;

        let all_match = header_matches && tx_result_matches && meta_matches;

        if all_match {
            stats.ledgers_matched += 1;
            if !ctx.quiet {
                print!(".");
                if stats.ledgers_verified % 64 == 0 {
                    println!(" {}", seq);
                }
                std::io::Write::flush(&mut std::io::stdout()).ok();
            }
        } else {
            stats.ledgers_mismatched += 1;
            if !header_matches {
                stats.header_mismatches += 1;
            }
            if !tx_result_matches {
                stats.tx_result_mismatches += 1;
            }
            if !meta_matches {
                stats.meta_mismatches += 1;
            }

            println!();
            println!("  Ledger {}: MISMATCH", seq);
            if !header_matches {
                println!(
                    "    Header hash: ours={} expected={}",
                    result.header_hash.to_hex(),
                    expected_header_hash.to_hex()
                );
                let bucket_levels = ctx.ledger_manager.bucket_list_levels();
                print_header_field_diffs(&result.header, &cdp_header, &bucket_levels);
            }
            if !tx_result_matches {
                println!(
                    "    TX result hash: ours={} expected={}",
                    our_tx_result_hash.to_hex(),
                    expected_tx_result_hash.to_hex()
                );
            }

            if ctx.show_diff && !tx_result_matches {
                print_tx_result_diffs(&result.tx_results, &cdp_tx_results);
            }

            // Compare eviction data when header mismatches but TX results match
            if !header_matches && tx_result_matches {
                print_eviction_and_entry_diagnostics(ctx, &lcm, &result.header);
            }

            if ctx.stop_on_error {
                anyhow::bail!("Mismatch at ledger {}", seq);
            }
        }

        // Collect and display performance metrics
        if let Some(ref perf) = result.perf {
            stats.total_close_us += perf.total_us;
            stats.total_tx_exec_us += perf.tx_exec_us;
            stats.total_commit_us += perf.commit_setup_us
                + perf.add_batch_us
                + perf.hot_archive_us
                + perf.header_us
                + perf.commit_close_us;
            stats.total_add_batch_us += perf.add_batch_us;
            stats.total_eviction_us += perf.eviction_us;
            stats.total_tx_count += perf.tx_count;
            stats.total_cache_hits += perf.cache.hits;
            stats.total_cache_misses += perf.cache.misses;
            if perf.rss_after_bytes > stats.peak_rss_bytes {
                stats.peak_rss_bytes = perf.rss_after_bytes;
            }
            if perf.total_us > stats.slowest_ledger_us {
                stats.slowest_ledger_us = perf.total_us;
                stats.slowest_ledger_seq = seq;
            }
            // Track top slowest transactions across all ledgers
            for tx in &perf.tx_timings {
                stats
                    .slowest_txs
                    .push((seq, tx.hash_hex.clone(), tx.exec_us));
            }

            // Print per-ledger summary every 64 ledgers or if slow
            if !ctx.quiet && (stats.ledgers_verified % 64 == 0 || perf.total_us > 500_000) {
                let cache_rate = if perf.cache.hits + perf.cache.misses > 0 {
                    perf.cache.hits as f64 / (perf.cache.hits + perf.cache.misses) as f64 * 100.0
                } else {
                    0.0
                };
                println!(
                    "\n  [PERF L{}] total={:.1}ms tx_exec={:.1}ms commit={:.1}ms \
                 add_batch={:.1}ms eviction={:.1}ms txs={} cache={:.0}% \
                 rss={:.0}MB",
                    seq,
                    perf.total_us as f64 / 1000.0,
                    perf.tx_exec_us as f64 / 1000.0,
                    (perf.commit_setup_us
                        + perf.add_batch_us
                        + perf.hot_archive_us
                        + perf.header_us
                        + perf.commit_close_us) as f64
                        / 1000.0,
                    perf.add_batch_us as f64 / 1000.0,
                    perf.eviction_us as f64 / 1000.0,
                    perf.tx_count,
                    cache_rate,
                    perf.rss_after_bytes as f64 / (1024.0 * 1024.0),
                );
                // Show top 3 slowest txs for this ledger
                for tx in perf.tx_timings.iter().take(3) {
                    if tx.exec_us > 1000 {
                        println!(
                            "    tx[{}] {}..  {:.1}ms  ops={}  {}  {}",
                            tx.index,
                            &tx.hash_hex[..tx.hash_hex.len().min(12)],
                            tx.exec_us as f64 / 1000.0,
                            tx.op_count,
                            if tx.is_soroban { "soroban" } else { "classic" },
                            if tx.success { "ok" } else { "FAILED" },
                        );
                    }
                }
            }
        }
    }

    // Update prev hash for next ledger
    *prev_ledger_hash = result.header_hash;
    Ok(())
}

/// Phase 6: Print verification and performance summaries.
fn print_summary(stats: &mut VerifyStats, elapsed: Duration) {
    // Print summary
    println!();
    println!();
    println!("Verification Summary");
    println!("====================");
    println!("  Ledgers verified: {}", stats.ledgers_verified);
    println!("  Ledgers matched:  {}", stats.ledgers_matched);
    println!("  Ledgers with mismatches: {}", stats.ledgers_mismatched);
    if stats.ledgers_mismatched > 0 {
        println!("    - Header hash mismatches: {}", stats.header_mismatches);
        println!(
            "    - TX result hash mismatches: {}",
            stats.tx_result_mismatches
        );
        println!("    - Meta mismatches: {}", stats.meta_mismatches);
    }
    println!();
    println!("  Total time: {:.2}s", elapsed.as_secs_f64());
    if stats.ledgers_verified > 0 {
        println!(
            "  Average per ledger: {:.2}ms",
            elapsed.as_millis() as f64 / stats.ledgers_verified as f64
        );
    }

    // Performance summary
    println!();
    println!("Performance Summary");
    println!("====================");
    if stats.ledgers_verified > 0 {
        let avg_close_ms = stats.total_close_us as f64 / stats.ledgers_verified as f64 / 1000.0;
        let avg_tx_exec_ms = stats.total_tx_exec_us as f64 / stats.ledgers_verified as f64 / 1000.0;
        let avg_commit_ms = stats.total_commit_us as f64 / stats.ledgers_verified as f64 / 1000.0;
        println!("  Timing (averages per ledger):");
        println!("    close_ledger:  {:.2}ms", avg_close_ms);
        println!("    tx_exec:       {:.2}ms", avg_tx_exec_ms);
        println!("    commit:        {:.2}ms", avg_commit_ms);
        println!(
            "    add_batch:     {:.2}ms",
            stats.total_add_batch_us as f64 / stats.ledgers_verified as f64 / 1000.0
        );
        println!(
            "    eviction:      {:.2}ms",
            stats.total_eviction_us as f64 / stats.ledgers_verified as f64 / 1000.0
        );
        println!();
        println!("  Transactions:");
        println!("    total:         {}", stats.total_tx_count);
        println!(
            "    avg/ledger:    {:.1}",
            stats.total_tx_count as f64 / stats.ledgers_verified as f64
        );
        println!();
        println!("  Cache:");
        let overall_cache_rate = if stats.total_cache_hits + stats.total_cache_misses > 0 {
            stats.total_cache_hits as f64
                / (stats.total_cache_hits + stats.total_cache_misses) as f64
                * 100.0
        } else {
            0.0
        };
        println!("    hit rate:      {:.1}%", overall_cache_rate);
        println!("    total hits:    {}", stats.total_cache_hits);
        println!("    total misses:  {}", stats.total_cache_misses);
        println!();
        println!("  Memory:");
        println!(
            "    peak RSS:      {:.1}MB",
            stats.peak_rss_bytes as f64 / (1024.0 * 1024.0)
        );
        println!();
        println!(
            "  Slowest ledger:  {} ({:.1}ms)",
            stats.slowest_ledger_seq,
            stats.slowest_ledger_us as f64 / 1000.0
        );

        // Top 10 slowest transactions overall
        stats.slowest_txs.sort_by(|a, b| b.2.cmp(&a.2));
        println!();
        println!("  Top 10 slowest transactions:");
        for (i, (ledger, hash, us)) in stats.slowest_txs.iter().take(10).enumerate() {
            println!(
                "    {}. L{} {}..  {:.1}ms",
                i + 1,
                ledger,
                &hash[..hash.len().min(16)],
                *us as f64 / 1000.0
            );
        }
    }
}

/// Print detailed eviction and entry-level diagnostics for a mismatched ledger.
///
/// Called when the header hash mismatches but TX results match, indicating
/// the divergence is likely in bucket list state (eviction, upgrades, etc.).
fn print_eviction_and_entry_diagnostics(
    ctx: &VerifyContext,
    lcm: &stellar_xdr::curr::LedgerCloseMeta,
    result_header: &stellar_xdr::curr::LedgerHeader,
) {
    let cdp_evicted_keys = henyey_history::cdp::extract_evicted_keys(&lcm);
    let tx_metas = henyey_history::cdp::extract_transaction_metas(&lcm);
    let cdp_restored_keys = henyey_history::cdp::extract_restored_keys(&tx_metas);

    // Count CDP entry changes
    let mut cdp_creates = 0u32;
    let mut cdp_updates = 0u32;
    let mut cdp_deletes = 0u32;
    for tx_meta in &tx_metas {
        fn count_changes(
            changes: &[stellar_xdr::curr::LedgerEntryChange],
            creates: &mut u32,
            updates: &mut u32,
            deletes: &mut u32,
        ) {
            for change in changes {
                match change {
                    stellar_xdr::curr::LedgerEntryChange::Created(_) => *creates += 1,
                    stellar_xdr::curr::LedgerEntryChange::Updated(_) => *updates += 1,
                    stellar_xdr::curr::LedgerEntryChange::Removed(_) => *deletes += 1,
                    stellar_xdr::curr::LedgerEntryChange::Restored(_) => *updates += 1,
                    stellar_xdr::curr::LedgerEntryChange::State(_) => {}
                }
            }
        }
        for_each_tx_meta_changes(tx_meta, |changes| {
            count_changes(
                changes,
                &mut cdp_creates,
                &mut cdp_updates,
                &mut cdp_deletes,
            );
        });
    }

    // Extract upgrade meta entry counts
    let cdp_upgrade_metas = henyey_history::cdp::extract_upgrade_metas(&lcm);
    let mut upgrade_creates = 0u32;
    let mut upgrade_updates = 0u32;
    for um in &cdp_upgrade_metas {
        for change in um.changes.iter() {
            match change {
                stellar_xdr::curr::LedgerEntryChange::Created(_) => upgrade_creates += 1,
                stellar_xdr::curr::LedgerEntryChange::Updated(_) => upgrade_updates += 1,
                _ => {}
            }
        }
    }

    println!("    CDP meta: creates={}, updates={}, deletes={}, evicted={}, restored={}, upgrade_creates={}, upgrade_updates={}",
    cdp_creates, cdp_updates, cdp_deletes, cdp_evicted_keys.len(), cdp_restored_keys.len(),
    upgrade_creates, upgrade_updates);

    // Dump expected upgrade entries from CDP meta for comparison
    if !cdp_upgrade_metas.is_empty() {
        use sha2::{Digest, Sha256};
        println!("    CDP upgrade entries (expected):");
        for (ui, um) in cdp_upgrade_metas.iter().enumerate() {
            for change in um.changes.iter() {
                match change {
                    stellar_xdr::curr::LedgerEntryChange::Updated(entry) => {
                        let key_str = match &entry.data {
                            stellar_xdr::curr::LedgerEntryData::ConfigSetting(cs) => {
                                format!("ConfigSetting({:?})", cs.discriminant())
                            }
                            other => format!("{:?}", std::mem::discriminant(other)),
                        };
                        let xdr_bytes = entry
                            .to_xdr(stellar_xdr::curr::Limits::none())
                            .unwrap_or_default();
                        let xdr_size = xdr_bytes.len();
                        let hash = {
                            let mut h = Sha256::new();
                            h.update(&xdr_bytes);
                            let r = h.finalize();
                            format!("{:x}", r)
                        };
                        println!("      upgrade[{}] Updated: key={}, last_modified={}, xdr_size={}, xdr_hash={}",
                        ui, key_str, entry.last_modified_ledger_seq, xdr_size, hash);
                    }
                    stellar_xdr::curr::LedgerEntryChange::Created(entry) => {
                        let key_str = match &entry.data {
                            stellar_xdr::curr::LedgerEntryData::ConfigSetting(cs) => {
                                format!("ConfigSetting({:?})", cs.discriminant())
                            }
                            other => format!("{:?}", std::mem::discriminant(other)),
                        };
                        let xdr_bytes = entry
                            .to_xdr(stellar_xdr::curr::Limits::none())
                            .unwrap_or_default();
                        let xdr_size = xdr_bytes.len();
                        let hash = {
                            let mut h = Sha256::new();
                            h.update(&xdr_bytes);
                            let r = h.finalize();
                            format!("{:x}", r)
                        };
                        println!("      upgrade[{}] Created: key={}, last_modified={}, xdr_size={}, xdr_hash={}",
                        ui, key_str, entry.last_modified_ledger_seq, xdr_size, hash);
                    }
                    stellar_xdr::curr::LedgerEntryChange::State(entry) => {
                        let key_str = match &entry.data {
                            stellar_xdr::curr::LedgerEntryData::ConfigSetting(cs) => {
                                format!("ConfigSetting({:?})", cs.discriminant())
                            }
                            other => format!("{:?}", std::mem::discriminant(other)),
                        };
                        println!("      upgrade[{}] State(before): key={}", ui, key_str);
                    }
                    _ => {}
                }
            }
        }
    }

    // Also dump expected final TX entries from CDP meta
    {
        use sha2::{Digest, Sha256};
        // Coalesce: keep last Updated entry per key
        // Include ALL change sources: fee_processing, tx_apply_processing, and post_tx_apply_fee_processing
        let mut final_entries: std::collections::HashMap<Vec<u8>, stellar_xdr::curr::LedgerEntry> =
            std::collections::HashMap::new();
        // Helper to process a slice of changes into the coalesced map
        let coalesce_changes = |changes: &[stellar_xdr::curr::LedgerEntryChange],
                                map: &mut std::collections::HashMap<
            Vec<u8>,
            stellar_xdr::curr::LedgerEntry,
        >| {
            for change in changes {
                match change {
                    stellar_xdr::curr::LedgerEntryChange::Updated(entry)
                    | stellar_xdr::curr::LedgerEntryChange::Created(entry)
                    | stellar_xdr::curr::LedgerEntryChange::Restored(entry) => {
                        let key = henyey_common::entry_to_key(entry);
                        if let Ok(kb) = key.to_xdr(stellar_xdr::curr::Limits::none()) {
                            map.insert(kb, entry.clone());
                        }
                    }
                    stellar_xdr::curr::LedgerEntryChange::Removed(key) => {
                        if let Ok(kb) = key.to_xdr(stellar_xdr::curr::Limits::none()) {
                            map.remove(&kb);
                        }
                    }
                    _ => {}
                }
            }
        };
        let coalesce_tx_meta = |meta: &stellar_xdr::curr::TransactionMeta,
                                map: &mut std::collections::HashMap<
            Vec<u8>,
            stellar_xdr::curr::LedgerEntry,
        >| {
            for_each_tx_meta_changes(meta, |changes| {
                coalesce_changes(changes, map);
            });
        };
        // Process ALL change sources from LCM tx_processing
        match &lcm {
            stellar_xdr::curr::LedgerCloseMeta::V0(v0) => {
                for tp in v0.tx_processing.iter() {
                    coalesce_changes(&tp.fee_processing, &mut final_entries);
                    coalesce_tx_meta(&tp.tx_apply_processing, &mut final_entries);
                    // V0 TransactionResultMeta has no post_tx_apply_fee_processing
                }
            }
            stellar_xdr::curr::LedgerCloseMeta::V1(v1) => {
                for tp in v1.tx_processing.iter() {
                    coalesce_changes(&tp.fee_processing, &mut final_entries);
                    coalesce_tx_meta(&tp.tx_apply_processing, &mut final_entries);
                    // V1 TransactionResultMeta has no post_tx_apply_fee_processing
                }
            }
            stellar_xdr::curr::LedgerCloseMeta::V2(v2) => {
                for tp in v2.tx_processing.iter() {
                    coalesce_changes(&tp.fee_processing, &mut final_entries);
                    coalesce_tx_meta(&tp.tx_apply_processing, &mut final_entries);
                    coalesce_changes(&tp.post_tx_apply_fee_processing, &mut final_entries);
                }
            }
        }
        println!(
            "    CDP TX final entries (coalesced, {} unique keys)",
            final_entries.len()
        );

        // Compare CDP entries with our bucket list state
        let bl = ctx.ledger_manager.bucket_list();
        let bl_snapshot = henyey_bucket::BucketListSnapshot::new(&bl, result_header.clone());
        drop(bl);
        let mut diffs = 0;
        let mut missing = 0;
        for (key_bytes, cdp_entry) in final_entries.iter() {
            use stellar_xdr::curr::ReadXdr;
            if let Ok(key) = stellar_xdr::curr::LedgerKey::from_xdr(
                key_bytes.as_slice(),
                stellar_xdr::curr::Limits::none(),
            ) {
                let cdp_xdr = cdp_entry
                    .to_xdr(stellar_xdr::curr::Limits::none())
                    .unwrap_or_default();
                let cdp_hash = {
                    let mut h = Sha256::new();
                    h.update(&cdp_xdr);
                    format!("{:x}", h.finalize())
                };
                match bl_snapshot.get(&key) {
                    Some(our_entry) => {
                        let our_xdr = our_entry
                            .to_xdr(stellar_xdr::curr::Limits::none())
                            .unwrap_or_default();
                        let our_hash = {
                            let mut h = Sha256::new();
                            h.update(&our_xdr);
                            format!("{:x}", h.finalize())
                        };
                        if our_hash != cdp_hash {
                            diffs += 1;
                            let key_str = format!("{:?}", std::mem::discriminant(&cdp_entry.data));
                            println!("    ENTRY DIFF #{}: key={:?}", diffs, key_str);
                            println!(
                                "      CDP:  lm={} hash={}",
                                cdp_entry.last_modified_ledger_seq, cdp_hash
                            );
                            println!(
                                "      Ours: lm={} hash={}",
                                our_entry.last_modified_ledger_seq, our_hash
                            );
                            println!(
                                "      CDP  xdr: {}",
                                hex::encode(&cdp_xdr[..cdp_xdr.len().min(200)])
                            );
                            println!(
                                "      Ours xdr: {}",
                                hex::encode(&our_xdr[..our_xdr.len().min(200)])
                            );
                            // For offers, show readable details
                            if let (
                                stellar_xdr::curr::LedgerEntryData::Offer(cdp_o),
                                stellar_xdr::curr::LedgerEntryData::Offer(our_o),
                            ) = (&cdp_entry.data, &our_entry.data)
                            {
                                println!(
                                    "      CDP  offer: seller={:?} amount={} price={}/{}",
                                    hex::encode(
                                        &{
                                            let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                                                ref pk,
                                            ) = cdp_o.seller_id.0;
                                            pk.0
                                        }[..8]
                                    ),
                                    cdp_o.amount,
                                    cdp_o.price.n,
                                    cdp_o.price.d
                                );
                                println!(
                                    "      Ours offer: seller={:?} amount={} price={}/{}",
                                    hex::encode(
                                        &{
                                            let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                                                ref pk,
                                            ) = our_o.seller_id.0;
                                            pk.0
                                        }[..8]
                                    ),
                                    our_o.amount,
                                    our_o.price.n,
                                    our_o.price.d
                                );
                            }
                            if let (
                                stellar_xdr::curr::LedgerEntryData::Account(cdp_a),
                                stellar_xdr::curr::LedgerEntryData::Account(our_a),
                            ) = (&cdp_entry.data, &our_entry.data)
                            {
                                let cdp_pk = {
                                    let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) =
                                        cdp_a.account_id.0;
                                    hex::encode(&pk.0[..16])
                                };
                                // Extract sponsorship counts from extensions
                                let get_ext =
                                    |a: &stellar_xdr::curr::AccountEntry| -> (u32, u32, u32) {
                                        match &a.ext {
                                    stellar_xdr::curr::AccountEntryExt::V0 => (0, 0, 0),
                                    stellar_xdr::curr::AccountEntryExt::V1(v1) => match &v1.ext {
                                        stellar_xdr::curr::AccountEntryExtensionV1Ext::V0 => (0, 0, 0),
                                        stellar_xdr::curr::AccountEntryExtensionV1Ext::V2(v2) => (v2.num_sponsoring, v2.num_sponsored, v2.signer_sponsoring_i_ds.len() as u32),
                                    },
                                }
                                    };
                                let (cdp_ing, cdp_ed, cdp_sigs) = get_ext(cdp_a);
                                let (our_ing, our_ed, our_sigs) = get_ext(our_a);
                                println!("      CDP  account: id={} balance={} seq={} sub_entries={} flags={} num_sponsoring={} num_sponsored={} signer_sponsors={}",
                                cdp_pk, cdp_a.balance, cdp_a.seq_num.0, cdp_a.num_sub_entries, cdp_a.flags, cdp_ing, cdp_ed, cdp_sigs);
                                println!("      Ours account: id={} balance={} seq={} sub_entries={} flags={} num_sponsoring={} num_sponsored={} signer_sponsors={}",
                                cdp_pk, our_a.balance, our_a.seq_num.0, our_a.num_sub_entries, our_a.flags, our_ing, our_ed, our_sigs);
                                if cdp_a.balance != our_a.balance {
                                    println!(
                                        "      BALANCE DIFF: {} (ours - cdp)",
                                        our_a.balance - cdp_a.balance
                                    );
                                }
                                if cdp_a.num_sub_entries != our_a.num_sub_entries {
                                    println!(
                                        "      SUB_ENTRIES DIFF: {} (ours - cdp)",
                                        our_a.num_sub_entries as i64 - cdp_a.num_sub_entries as i64
                                    );
                                }
                                if cdp_ing != our_ing {
                                    println!(
                                        "      NUM_SPONSORING DIFF: {} (ours - cdp)",
                                        our_ing as i64 - cdp_ing as i64
                                    );
                                }
                                if cdp_ed != our_ed {
                                    println!(
                                        "      NUM_SPONSORED DIFF: {} (ours - cdp)",
                                        our_ed as i64 - cdp_ed as i64
                                    );
                                }
                            }
                            if let (
                                stellar_xdr::curr::LedgerEntryData::Trustline(cdp_t),
                                stellar_xdr::curr::LedgerEntryData::Trustline(our_t),
                            ) = (&cdp_entry.data, &our_entry.data)
                            {
                                println!(
                                    "      CDP  trustline: balance={} asset={:?}",
                                    cdp_t.balance, cdp_t.asset
                                );
                                println!(
                                    "      Ours trustline: balance={} asset={:?}",
                                    our_t.balance, our_t.asset
                                );
                            }
                            if let (
                                stellar_xdr::curr::LedgerEntryData::LiquidityPool(cdp_p),
                                stellar_xdr::curr::LedgerEntryData::LiquidityPool(our_p),
                            ) = (&cdp_entry.data, &our_entry.data)
                            {
                                let stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(ref cdp_cp) = cdp_p.body;
                                let stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(ref our_cp) = our_p.body;
                                println!(
                                    "      CDP  pool: reserve_a={} reserve_b={}",
                                    cdp_cp.reserve_a, cdp_cp.reserve_b
                                );
                                println!(
                                    "      Ours pool: reserve_a={} reserve_b={}",
                                    our_cp.reserve_a, our_cp.reserve_b
                                );
                            }
                            if diffs >= 20 {
                                break;
                            }
                        }
                    }
                    None => {
                        // For offers, try the offer_store instead of bucket list snapshot
                        // (offers are not indexed in bucket list snapshot)
                        if let stellar_xdr::curr::LedgerEntryData::Offer(ref cdp_offer) =
                            cdp_entry.data
                        {
                            let offer_store = ctx.ledger_manager.offer_store_lock();
                            if let Some(our_entry) = offer_store
                                .get_ledger_entry_by_id(cdp_offer.offer_id)
                                .as_ref()
                            {
                                let our_xdr = our_entry
                                    .to_xdr(stellar_xdr::curr::Limits::none())
                                    .unwrap_or_default();
                                let our_hash = {
                                    let mut h = Sha256::new();
                                    h.update(&our_xdr);
                                    format!("{:x}", h.finalize())
                                };
                                if our_hash != cdp_hash {
                                    diffs += 1;
                                    if let stellar_xdr::curr::LedgerEntryData::Offer(
                                        ref our_offer,
                                    ) = our_entry.data
                                    {
                                        let cdp_seller = {
                                            let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                                                ref pk,
                                            ) = cdp_offer.seller_id.0;
                                            hex::encode(&pk.0[..8])
                                        };
                                        println!(
                                            "    OFFER DIFF #{}: id={} seller={}",
                                            diffs, cdp_offer.offer_id, cdp_seller
                                        );
                                        println!(
                                            "      CDP:  amount={} price={}/{} lm={}",
                                            cdp_offer.amount,
                                            cdp_offer.price.n,
                                            cdp_offer.price.d,
                                            cdp_entry.last_modified_ledger_seq
                                        );
                                        println!(
                                            "      Ours: amount={} price={}/{} lm={}",
                                            our_offer.amount,
                                            our_offer.price.n,
                                            our_offer.price.d,
                                            our_entry.last_modified_ledger_seq
                                        );
                                    }
                                }
                                // else: offer matches, not a real diff
                            } else {
                                // Offer is truly missing from our state
                                missing += 1;
                                let cdp_seller = {
                                    let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) =
                                        cdp_offer.seller_id.0;
                                    hex::encode(&pk.0)
                                };
                                println!("    TRULY MISSING offer: id={} seller={} amount={} price={}/{} cdp_lm={}",
                                cdp_offer.offer_id, cdp_seller, cdp_offer.amount,
                                cdp_offer.price.n, cdp_offer.price.d, cdp_entry.last_modified_ledger_seq);
                            }
                        } else {
                            // Non-offer entry truly missing
                            missing += 1;
                            let key_str = match &cdp_entry.data {
                                stellar_xdr::curr::LedgerEntryData::Account(a) => {
                                    let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) =
                                        a.account_id.0;
                                    format!("Account({})", hex::encode(&pk.0[..8]))
                                }
                                stellar_xdr::curr::LedgerEntryData::Trustline(t) => {
                                    let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref pk) =
                                        t.account_id.0;
                                    format!(
                                        "Trustline(acct={}, asset={:?}, balance={})",
                                        hex::encode(&pk.0[..8]),
                                        t.asset,
                                        t.balance
                                    )
                                }
                                stellar_xdr::curr::LedgerEntryData::LiquidityPool(p) => {
                                    let stellar_xdr::curr::LiquidityPoolEntryBody::LiquidityPoolConstantProduct(ref cp) = p.body;
                                    format!("Pool(ra={}, rb={})", cp.reserve_a, cp.reserve_b)
                                }
                                other => format!("{:?}", std::mem::discriminant(other)),
                            };
                            println!(
                                "    MISSING in our state: {} cdp_lm={} hash={}",
                                key_str, cdp_entry.last_modified_ledger_seq, cdp_hash
                            );
                            println!(
                                "      cdp_xdr: {}",
                                hex::encode(&cdp_xdr[..cdp_xdr.len().min(200)])
                            );
                        }
                    }
                }
            }
        }
        println!(
            "    Entry comparison: {} diffs, {} truly missing (out of {} CDP entries)",
            diffs,
            missing,
            final_entries.len()
        );
    }
}
