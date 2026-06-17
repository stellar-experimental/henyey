//! Offline execution verification — replays ledgers against CDP data.
//!
//! Module entry point: the shared verification types (`VerifyExecutionOptions` /
//! `VerifyContext` / `VerifyStats`), the public `cmd_verify_execution` orchestrator,
//! and `setup` (the context builder). The diagnostic printers live in [`diffs`] and
//! the per-ledger verification loop / helpers live in [`run`]. Split out of the
//! former single-file `verify_execution.rs` (#3357); no logic change.

mod diffs;
mod run;

use std::sync::Arc;
use std::time::Instant;

use henyey_app::AppConfig;
use henyey_bucket::{BucketList, BucketManager, HotArchiveBucketList, PendingMergeState};
use henyey_common::Hash256;
use henyey_history::cdp::CachedCdpDataLake;
use henyey_history::checkpoint;
use henyey_history::verify;
use henyey_ledger::{LedgerManager, LedgerManagerConfig};

use run::{print_summary, run_verification_loop};

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
    _bucket_manager: Arc<BucketManager>,
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

    let live_next_states: Vec<Option<PendingMergeState>> = init_has.live_next_states()?;

    // Extract hot archive bucket hashes (protocol 23+)
    let hot_archive_hashes = init_has.hot_archive_bucket_hash_pairs();

    let hot_archive_next_states: Option<Vec<Option<PendingMergeState>>> =
        init_has.hot_archive_next_states()?;

    // Collect all bucket hashes to download
    let mut all_hashes: Vec<Hash256> = Vec::new();
    for (curr, snap) in &bucket_hashes {
        all_hashes.push(*curr);
        all_hashes.push(*snap);
    }
    for merge_state in live_next_states.iter().flatten() {
        all_hashes.extend(merge_state.referenced_hashes().copied());
    }
    if let Some(ref ha_hashes) = hot_archive_hashes {
        for (curr, snap) in ha_hashes {
            all_hashes.push(*curr);
            all_hashes.push(*snap);
        }
    }
    if let Some(ref ha_states) = hot_archive_next_states {
        for merge_state in ha_states.iter().flatten() {
            all_hashes.extend(merge_state.referenced_hashes().copied());
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
            |hash| bucket_manager.load_bucket_for_merge(hash),
            true, // restart_structure_based = true to match stellar-core online mode
        )
        .await?;

    if let Some(ref ha_next_states) = hot_archive_next_states {
        hot_archive_bucket_list.restart_merges_from_has(
            init_checkpoint,
            init_protocol_version,
            ha_next_states,
            |hash| bucket_manager.load_hot_archive_bucket_for_merge(hash),
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
    let init_header_hash = verify::verify_ledger_header_history_entry(&init_header_entry)?;
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
        _bucket_manager: bucket_manager,
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
