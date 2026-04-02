//! Execution-based ledger replay.
//!
//! Re-executes transactions against the current bucket list state to
//! reconstruct ledger entry changes. This is the default approach used
//! during catchup.

use crate::{is_checkpoint_ledger, verify, HistoryError, Result};
use henyey_bucket::{EvictionIterator, EvictionIteratorExt};
use henyey_common::protocol::{
    protocol_version_is_before, protocol_version_starts_from, ProtocolVersion,
};
use henyey_common::Hash256;
use henyey_ledger::{
    execution::{execute_transaction_set, load_soroban_config, SorobanContext},
    prepend_fee_event, EntryChange, LedgerDelta, LedgerError, LedgerSnapshot, SnapshotHandle,
    TransactionSetVariant,
};
use henyey_tx::{muxed_to_account_id, LedgerContext, TransactionFrame};
use sha2::{Digest, Sha256};
use stellar_xdr::curr::{
    BucketListType, ConfigSettingEntry, ConfigSettingId, LedgerEntry, LedgerEntryData,
    LedgerEntryExt, LedgerHeader, LedgerKey, LedgerKeyConfigSetting, StateArchivalSettings,
    TransactionResultSet, WriteXdr,
};

use super::diff::log_tx_result_mismatch;
use super::{LedgerReplayResult, ReplayConfig, ReplayExecutionContext};

fn load_state_archival_settings(snapshot: &SnapshotHandle) -> Option<StateArchivalSettings> {
    let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: ConfigSettingId::StateArchival,
    });
    match snapshot.get_entry(&key) {
        Ok(Some(entry)) => match entry.data {
            LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(settings)) => {
                Some(settings)
            }
            _ => None,
        },
        _ => None,
    }
}

/// Compute a hex-encoded SHA-256 digest over the XDR encoding of a slice of
/// `WriteXdr` items.  Used for debug logging only.
fn debug_xdr_hash<T: WriteXdr>(items: &[T]) -> String {
    let mut hasher = Sha256::new();
    for item in items {
        if let Ok(xdr) = item.to_xdr(stellar_xdr::curr::Limits::none()) {
            hasher.update(&xdr);
        }
    }
    hex::encode(hasher.finalize())
}

/// Result of running the incremental eviction scan for a single ledger.
struct EvictionScanResult {
    evicted_keys: Vec<LedgerKey>,
    archived_entries: Vec<LedgerEntry>,
    updated_iterator: Option<EvictionIterator>,
    ran: bool,
}

struct EvictionScanContext<'a> {
    config: &'a ReplayConfig,
    header: &'a LedgerHeader,
    eviction_iterator: Option<EvictionIterator>,
    eviction_settings: &'a StateArchivalSettings,
    init_entries: &'a [LedgerEntry],
    live_entries: &'a [LedgerEntry],
    dead_entries: &'a [LedgerKey],
}

/// Run incremental eviction scan for protocol 23+ and resolve candidates.
///
/// Returns evicted keys, archived entries, and the updated iterator position.
/// If eviction is disabled or the protocol version is too low, returns an empty result.
fn run_eviction_scan(
    bucket_list: &mut henyey_bucket::BucketList,
    context: EvictionScanContext<'_>,
) -> Result<EvictionScanResult> {
    if !context.config.run_eviction
        || protocol_version_is_before(context.header.ledger_version, ProtocolVersion::V23)
    {
        return Ok(EvictionScanResult {
            evicted_keys: Vec::new(),
            archived_entries: Vec::new(),
            updated_iterator: context.eviction_iterator,
            ran: false,
        });
    }

    let iter = context.eviction_iterator.unwrap_or_else(|| {
        EvictionIterator::new(context.eviction_settings.starting_eviction_scan_level)
    });
    let eviction_result = bucket_list
        .scan_for_eviction_incremental(iter, context.header.ledger_seq, context.eviction_settings)
        .map_err(HistoryError::Bucket)?;

    tracing::info!(
        ledger_seq = context.header.ledger_seq,
        bytes_scanned = eviction_result.bytes_scanned,
        candidates = eviction_result.candidates.len(),
        end_level = eviction_result.end_iterator.bucket_list_level,
        end_is_curr = eviction_result.end_iterator.is_curr_bucket,
        "Incremental eviction scan results"
    );

    // Resolution phase: apply TTL filtering + live-entry invalidation +
    // max_entries limit.
    // This matches stellar-core resolveBackgroundEvictionScan which:
    // 1. Filters out entries whose TTL was modified by TXs in this ledger
    // 2. Checks for (and logs) modified live entries without TTL changes
    // 3. Evicts up to maxEntriesToArchive entries
    // 4. Sets iterator based on whether the limit was hit
    //
    // Parity: stellar-core passes `ltx.getAllKeysWithoutSealing()` which
    // contains ALL modified keys. We build the equivalent.
    let modified_keys: std::collections::HashSet<LedgerKey> = context
        .init_entries
        .iter()
        .chain(context.live_entries.iter())
        .map(|entry| henyey_common::entry_to_key(entry))
        .chain(context.dead_entries.iter().cloned())
        .collect();

    let resolved = eviction_result.resolve(
        context.eviction_settings.max_entries_to_archive,
        &modified_keys,
    );

    Ok(EvictionScanResult {
        evicted_keys: resolved.evicted_keys,
        archived_entries: resolved.archived_entries,
        updated_iterator: Some(resolved.end_iterator),
        ran: true,
    })
}

/// Verify the combined bucket list hash at checkpoint boundaries.
///
/// Verification is only reliable at checkpoints (ledger_seq % 64 == 63) because
/// re-execution without TransactionMeta may produce slightly different entry values.
/// For protocol 23+, eviction must also be running to get accurate results.
fn verify_bucket_list_hash(
    config: &ReplayConfig,
    header: &LedgerHeader,
    bucket_list: &henyey_bucket::BucketList,
    hot_archive_bucket_list: &henyey_bucket::HotArchiveBucketList,
    eviction_iterator: Option<EvictionIterator>,
) -> Result<()> {
    let is_checkpoint = header.ledger_seq % 64 == 63;
    let eviction_running = config.run_eviction && eviction_iterator.is_some();
    let can_verify = is_checkpoint
        && (protocol_version_is_before(header.ledger_version, ProtocolVersion::V23)
            || eviction_running);

    if !can_verify {
        tracing::debug!(
            ledger_seq = header.ledger_seq,
            protocol_version = header.ledger_version,
            is_checkpoint = is_checkpoint,
            eviction_running = eviction_running,
            "Skipping bucket list verification (only verified at checkpoints)"
        );
        return Ok(());
    }

    let expected = Hash256::from(header.bucket_list_hash.0);
    tracing::info!(
        ledger_seq = header.ledger_seq,
        protocol_version = header.ledger_version,
        expected_hash = %expected.to_hex(),
        "Verifying bucket list hash"
    );
    let actual =
        combined_bucket_list_hash(bucket_list, hot_archive_bucket_list, header.ledger_version);
    if actual != expected {
        tracing::error!(
            ledger_seq = header.ledger_seq,
            expected_hash = %expected.to_hex(),
            actual_hash = %actual.to_hex(),
            "Bucket list hash mismatch - logging detailed state"
        );
        for level in 0..henyey_bucket::BUCKET_LIST_LEVELS {
            let level_ref = bucket_list.level(level).unwrap();
            let curr_hash = level_ref.curr.hash();
            let snap_hash = level_ref.snap.hash();
            let level_hash = level_ref.hash();
            tracing::error!(
                level = level,
                curr_hash = %curr_hash,
                curr_entries = level_ref.curr.len(),
                snap_hash = %snap_hash,
                snap_entries = level_ref.snap.len(),
                level_hash = %level_hash,
                "Level state at mismatch"
            );
        }
        return Err(HistoryError::VerificationFailed(format!(
            "bucket list hash mismatch at ledger {} protocol {} (expected {}, got {})",
            header.ledger_seq,
            header.ledger_version,
            expected.to_hex(),
            actual.to_hex()
        )));
    }
    Ok(())
}

/// Classify delta entries into INIT, LIVE, and DEAD categories.
///
/// Entries that the delta marks as INIT but that already exist in the live bucket
/// list (e.g., shared WASM restored from hot archive) are moved to LIVE. Entries
/// restored from hot archive are added to LIVE, and dead entries corresponding to
/// restored keys are removed.
fn classify_delta_entries(
    header: &LedgerHeader,
    bucket_list: &henyey_bucket::BucketList,
    delta_init_entries: Vec<LedgerEntry>,
    mut live_entries: Vec<LedgerEntry>,
    mut dead_entries: Vec<LedgerKey>,
    hot_archive_restored_keys: &[LedgerKey],
) -> (Vec<LedgerEntry>, Vec<LedgerEntry>, Vec<LedgerKey>) {
    let delta_init_count = delta_init_entries.len();
    let delta_live_count = live_entries.len();

    // Check if "init" entries already exist in the LIVE bucket list.
    // This can happen when restoring from hot archive when another contract still uses
    // the same ContractCode (shared WASM). Only check ContractCode and ContractData.
    let mut init_entries: Vec<LedgerEntry> = Vec::new();
    let mut moved_to_live_count = 0u32;
    for entry in delta_init_entries {
        let key = henyey_common::entry_to_key(&entry);

        let should_check = matches!(
            &entry.data,
            stellar_xdr::curr::LedgerEntryData::ContractCode(_)
                | stellar_xdr::curr::LedgerEntryData::ContractData(_)
        );

        let already_in_bucket_list = should_check && bucket_list.get(&key).ok().flatten().is_some();

        if already_in_bucket_list {
            tracing::debug!(
                ledger_seq = header.ledger_seq,
                key_type = ?std::mem::discriminant(&key),
                "Moving INIT entry to LIVE - already exists in bucket list"
            );
            live_entries.push(entry);
            moved_to_live_count += 1;
        } else {
            init_entries.push(entry);
        }
    }
    if moved_to_live_count > 0 || is_checkpoint_ledger(header.ledger_seq) {
        tracing::info!(
            ledger_seq = header.ledger_seq,
            delta_init_count = delta_init_count,
            final_init_count = init_entries.len(),
            moved_to_live_count = moved_to_live_count,
            delta_live_count = delta_live_count,
            final_live_count = live_entries.len(),
            delta_dead_count = dead_entries.len(),
            "Entry counts after INIT→LIVE check"
        );
    }

    // Handle hot archive restored entries.
    let init_entry_keys: std::collections::HashSet<_> = init_entries
        .iter()
        .map(|e| henyey_common::entry_to_key(e))
        .collect();
    for key in hot_archive_restored_keys {
        if init_entry_keys.contains(key) {
            continue;
        }
        if let Ok(Some(mut entry)) = bucket_list.get(key) {
            entry.last_modified_ledger_seq = header.ledger_seq;
            live_entries.push(entry);
        }
    }

    // Remove restored entries from dead_entries.
    if !hot_archive_restored_keys.is_empty() {
        let restored_set: std::collections::HashSet<_> = hot_archive_restored_keys.iter().collect();
        dead_entries.retain(|k| !restored_set.contains(k));
    }

    (init_entries, live_entries, dead_entries)
}

/// Compute the size of a Soroban entry for state size tracking.
///
/// Returns the entry size in bytes for ContractData/ContractCode entries,
/// or 0 for other entry types. ContractCode uses rent-adjusted size.
///
/// `cost_params` should be `Some((cpu_cost_params, mem_cost_params))` from the
/// on-chain ConfigSettingEntry to ensure correct compiled module memory cost
/// calculation. If `None`, falls back to `Budget::default()` which may produce
/// incorrect sizes.
pub(super) fn soroban_entry_size(
    entry: &LedgerEntry,
    protocol_version: u32,
    cost_params: Option<(
        &stellar_xdr::curr::ContractCostParams,
        &stellar_xdr::curr::ContractCostParams,
    )>,
) -> i64 {
    use henyey_tx::operations::execute::entry_size_for_rent_by_protocol_with_cost_params;
    use stellar_xdr::curr::WriteXdr;

    match &entry.data {
        LedgerEntryData::ContractData(_) => {
            // Contract data uses XDR size
            entry
                .to_xdr(stellar_xdr::curr::Limits::none())
                .map(|xdr_bytes| xdr_bytes.len() as i64)
                .unwrap_or(0)
        }
        LedgerEntryData::ContractCode(_) => {
            // Contract code uses rent-adjusted size (includes compiled module memory)
            entry
                .to_xdr(stellar_xdr::curr::Limits::none())
                .map(|xdr_bytes| {
                    let xdr_size = xdr_bytes.len() as u32;
                    entry_size_for_rent_by_protocol_with_cost_params(
                        protocol_version,
                        entry,
                        xdr_size,
                        cost_params,
                    ) as i64
                })
                .unwrap_or(0)
        }
        _ => 0,
    }
}

/// Compute the net change in Soroban state size from entry changes.
///
/// This processes the delta's changes to accurately compute the size difference:
/// - INIT (created): +size of new Soroban entries
/// - LIVE (updated): +(new_size - old_size) for modified Soroban entries
/// - DEAD (deleted): -size of removed Soroban entries
pub(super) fn compute_soroban_state_size_delta(
    changes: &[EntryChange],
    protocol_version: u32,
    cost_params: Option<(
        &stellar_xdr::curr::ContractCostParams,
        &stellar_xdr::curr::ContractCostParams,
    )>,
) -> i64 {
    let mut delta: i64 = 0;

    for change in changes {
        match change {
            EntryChange::Created(entry) => {
                delta += soroban_entry_size(entry, protocol_version, cost_params);
            }
            EntryChange::Updated { previous, current } => {
                let old_size = soroban_entry_size(previous, protocol_version, cost_params);
                let new_size = soroban_entry_size(current.as_ref(), protocol_version, cost_params);
                delta += new_size - old_size;
            }
            EntryChange::Deleted { previous } => {
                delta -= soroban_entry_size(previous, protocol_version, cost_params);
            }
        }
    }

    delta
}

/// Compute the LiveSorobanStateSizeWindow entry if it needs updating at this ledger.
///
/// The window tracks Soroban state size samples over time for resource limiting.
/// Updates occur when:
/// - The sample size changes (window is resized)
/// - We're at a sample boundary (seq % sample_period == 0)
pub(super) fn compute_soroban_state_size_window_entry(
    seq: u32,
    bucket_list: &henyey_bucket::BucketList,
    soroban_state_size: u64,
    archival_override: Option<&stellar_xdr::curr::StateArchivalSettings>,
) -> Option<LedgerEntry> {
    use stellar_xdr::curr::VecM;

    // Load StateArchival settings
    let archival = if let Some(override_settings) = archival_override {
        override_settings.clone()
    } else {
        let archival_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::StateArchival,
        });
        let archival_entry = bucket_list.get(&archival_key).ok()??;
        match archival_entry.data {
            LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(settings)) => settings,
            _ => return None,
        }
    };

    let sample_period = archival.live_soroban_state_size_window_sample_period;
    let sample_size = archival.live_soroban_state_size_window_sample_size as usize;
    if sample_period == 0 || sample_size == 0 {
        return None;
    }

    // Load current window
    let window_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: ConfigSettingId::LiveSorobanStateSizeWindow,
    });
    let window_entry = bucket_list.get(&window_key).ok()??;
    let window = match window_entry.data {
        LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(w)) => w,
        _ => return None,
    };

    let mut window_vec: Vec<u64> = window.into();
    if window_vec.is_empty() {
        return None;
    }

    let mut changed = false;

    // Resize window if sample size changed
    if window_vec.len() != sample_size {
        if sample_size < window_vec.len() {
            let remove_count = window_vec.len() - sample_size;
            window_vec.drain(0..remove_count);
        } else {
            let oldest = window_vec[0];
            let insert_count = sample_size - window_vec.len();
            for _ in 0..insert_count {
                window_vec.insert(0, oldest);
            }
        }
        changed = true;
    }

    // Sample at period boundary
    if seq % sample_period == 0 && !window_vec.is_empty() {
        window_vec.remove(0);
        window_vec.push(soroban_state_size);
        changed = true;
    }

    if !changed {
        return None;
    }

    let window_vecm: VecM<u64> = window_vec.try_into().ok()?;

    Some(LedgerEntry {
        last_modified_ledger_seq: seq,
        data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(
            window_vecm,
        )),
        ext: LedgerEntryExt::V0,
    })
}

fn combined_bucket_list_hash(
    live_bucket_list: &henyey_bucket::BucketList,
    hot_archive_bucket_list: &henyey_bucket::HotArchiveBucketList,
    protocol_version: u32,
) -> Hash256 {
    let live_hash = live_bucket_list.hash();
    if protocol_version_starts_from(protocol_version, ProtocolVersion::V23) {
        let hot_hash = hot_archive_bucket_list.hash();
        tracing::info!(
            live_hash = %live_hash,
            hot_archive_hash = %hot_hash,
            "Computing combined bucket list hash (protocol >= 23)"
        );
        let mut hasher = Sha256::new();
        hasher.update(live_hash.as_bytes());
        hasher.update(hot_hash.as_bytes());
        let result = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&result);
        Hash256::from_bytes(bytes)
    } else {
        tracing::info!(
            live_hash = %live_hash,
            "Using live bucket list hash only (protocol < 23)"
        );
        live_hash
    }
}

/// Replay a ledger by re-executing transactions against the current bucket list.
///
/// If `eviction_iterator` is provided, incremental eviction will be performed.
/// The updated iterator is returned in the result for use in subsequent ledgers.
///
/// # Arguments
///
/// * `header` - The ledger header being replayed
/// * `tx_set` - The transaction set to execute
pub fn replay_ledger_with_execution(
    header: &LedgerHeader,
    tx_set: &TransactionSetVariant,
    context: ReplayExecutionContext<'_>,
) -> Result<LedgerReplayResult> {
    let ReplayExecutionContext {
        bucket_list,
        hot_archive_bucket_list,
        network_id,
        config,
        expected_tx_results,
        eviction_iterator,
        module_cache,
        soroban_state_size,
        prev_id_pool,
        offer_entries,
    } = context;

    if config.verify_results {
        verify::verify_tx_set(header, tx_set)?;
    }

    // Resolve all pending async merges before cloning.
    // This ensures merge results are cached and won't be lost during clone.
    bucket_list.resolve_all_pending_merges();

    // Create snapshot with the correct id_pool from the previous ledger.
    // This is critical for correct offer ID assignment during transaction execution.
    let mut snapshot = LedgerSnapshot::empty(header.ledger_seq);
    snapshot.set_id_pool(prev_id_pool);
    let bucket_list_ref = std::sync::Arc::new(std::sync::RwLock::new(bucket_list.clone()));
    // Also include hot archive bucket list for archived entries and their TTLs.
    // Use parking_lot::RwLock<Option<...>> to match the type expected by execute_transaction_set.
    let hot_archive_ref: std::sync::Arc<
        parking_lot::RwLock<Option<henyey_bucket::HotArchiveBucketList>>,
    > = std::sync::Arc::new(parking_lot::RwLock::new(Some(
        hot_archive_bucket_list.clone(),
    )));
    let hot_archive_for_lookup = hot_archive_ref.clone();
    let lookup_fn = std::sync::Arc::new(move |key: &LedgerKey| {
        // First try the live bucket list
        if let Some(entry) = bucket_list_ref
            .read()
            .map_err(|_| LedgerError::Snapshot("bucket list lock poisoned".to_string()))?
            .get(key)
            .map_err(LedgerError::Bucket)?
        {
            return Ok(Some(entry));
        }
        // If not found, search hot archive for archived entries and TTLs
        let ha_guard = hot_archive_for_lookup.read();
        match ha_guard.as_ref() {
            Some(ha) => ha.get(key).map_err(LedgerError::Bucket),
            None => Ok(None),
        }
    });
    let mut snapshot = SnapshotHandle::with_lookup(snapshot, lookup_fn);

    // If offer entries are provided, set the entries_fn so that
    // load_orderbook_offers() can populate the executor's order book.
    // Without this, offer matching (ManageSellOffer, PathPayment, etc.) fails
    // because the executor starts with an empty order book.
    if let Some(offers) = offer_entries {
        let offers = std::sync::Arc::new(offers);
        let entries_fn: henyey_ledger::EntriesLookupFn =
            std::sync::Arc::new(move || Ok((*offers).clone()));
        snapshot.set_entries_lookup(entries_fn);
    }

    let mut delta = LedgerDelta::new(header.ledger_seq);
    let transactions = tx_set.transactions_with_base_fee();
    // Load SorobanConfig from ledger ConfigSettingEntry for accurate Soroban execution
    let soroban_config = load_soroban_config(&snapshot, header.ledger_version);
    // Save cost params before soroban_config is moved into execute_transaction_set
    let cpu_cost_params = soroban_config.cpu_cost_params.clone();
    let mem_cost_params = soroban_config.mem_cost_params.clone();
    let eviction_settings =
        load_state_archival_settings(&snapshot).unwrap_or(config.eviction_settings.clone());
    // Use transaction set hash as base PRNG seed for Soroban execution
    let soroban_base_prng_seed = tx_set.hash();
    let classic_events = henyey_tx::ClassicEventConfig {
        emit_classic_events: config.emit_classic_events,
        backfill_stellar_asset_events: config.backfill_stellar_asset_events,
    };
    let ledger_context = LedgerContext::new(
        header.ledger_seq,
        header.scp_value.close_time.0,
        header.base_fee,
        header.base_reserve,
        header.ledger_version,
        *network_id,
    );
    let mut tx_set_result = execute_transaction_set(
        &snapshot,
        &transactions,
        &ledger_context,
        &mut delta,
        SorobanContext {
            config: soroban_config,
            base_prng_seed: soroban_base_prng_seed.0,
            classic_events,
            module_cache,
            hot_archive: Some(hot_archive_ref.clone()),
            runtime_handle: None,
            soroban_state: None,
            offer_store: None,
            emit_soroban_tx_meta_ext_v1: false,
            enable_soroban_diagnostic_events: false,
        },
    )
    .map_err(|e| HistoryError::CatchupFailed(format!("replay execution failed: {}", e)))?;

    // Add fee events to transaction metadata (matching online mode behavior)
    if classic_events.events_enabled(header.ledger_version) {
        for (idx, ((envelope, _), meta)) in transactions
            .iter()
            .zip(tx_set_result.tx_result_metas.iter_mut())
            .enumerate()
        {
            let fee_charged = tx_set_result.tx_results[idx].result.fee_charged;
            let frame = TransactionFrame::with_network(envelope.clone(), *network_id);
            let fee_source = muxed_to_account_id(&frame.fee_source_account());
            prepend_fee_event(
                &mut meta.tx_apply_processing,
                &fee_source,
                fee_charged,
                header.ledger_version,
                network_id,
                classic_events,
            );
        }
    }

    if config.verify_results {
        let result_set =
            TransactionResultSet {
                results: tx_set_result.tx_results.clone().try_into().map_err(|_| {
                    HistoryError::CatchupFailed("tx result set too large".to_string())
                })?,
            };
        let xdr = result_set
            .to_xdr(stellar_xdr::curr::Limits::none())
            .map_err(|e| {
                HistoryError::CatchupFailed(format!("failed to encode tx result set: {}", e))
            })?;
        if let Err(err) = verify::verify_tx_result_set(header, &xdr) {
            if let Some(expected) = expected_tx_results {
                log_tx_result_mismatch(header, expected, &tx_set_result.tx_results, &transactions);
            }
            return Err(err);
        }
    }

    // Use historical fee_charged values from expected_tx_results when available.
    // During replay, our re-execution may calculate fees differently than the original
    // execution (e.g., due to subtle parity differences). The historical fee_pool in
    // the header was computed using the original fee_charged values, so we need to
    // use those values for correct state tracking.
    let fee_pool_delta = if let Some(expected_results) = expected_tx_results {
        expected_results.iter().map(|r| r.result.fee_charged).sum()
    } else {
        delta.fee_pool_delta()
    };
    let total_coins_delta = delta.total_coins_delta();
    let changes = delta.changes().cloned().collect::<Vec<_>>();
    // Classify delta entries: move INIT entries that already exist in the live
    // bucket list to LIVE, and reconcile hot-archive restored entries.
    let (init_entries, live_entries, dead_entries) = classify_delta_entries(
        header,
        bucket_list,
        delta.init_entries(),
        delta.live_entries(),
        delta.dead_entries(),
        &tx_set_result.hot_archive_restored_keys,
    );

    // Run incremental eviction scan for protocol 23+ before applying transaction changes
    // This matches stellar-core's behavior: eviction is determined by TTL state
    // from the current bucket list, then evicted entries are added as DEAD entries
    let eviction = run_eviction_scan(
        bucket_list,
        EvictionScanContext {
            config,
            header,
            eviction_iterator: eviction_iterator.clone(),
            eviction_settings: &eviction_settings,
            init_entries: &init_entries,
            live_entries: &live_entries,
            dead_entries: &dead_entries,
        },
    )?;

    // Combine transaction dead entries with evicted entries
    let mut all_dead_entries = dead_entries.clone();
    all_dead_entries.extend(eviction.evicted_keys);

    // Build live entries including eviction iterator update.
    // stellar-core updates the EvictionIterator ConfigSettingEntry EVERY ledger
    // during eviction scan. We do the same for consistency.
    let mut all_live_entries = live_entries.clone();
    if eviction.ran {
        if let Some(ref iter) = eviction.updated_iterator {
            let eviction_iter_entry = LedgerEntry {
                last_modified_ledger_seq: header.ledger_seq,
                data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::EvictionIterator(
                    iter.clone(),
                )),
                ext: LedgerEntryExt::V0,
            };
            all_live_entries.push(eviction_iter_entry);
            tracing::debug!(
                ledger_seq = header.ledger_seq,
                level = iter.bucket_list_level,
                is_curr = iter.is_curr_bucket,
                offset = iter.bucket_file_offset,
                "Added EvictionIterator entry to live entries"
            );
        }
    }

    // Update LiveSorobanStateSizeWindow if needed.
    // stellar-core calls snapshotSorobanStateSizeWindow() at the end of ledger close.
    // This samples the current Soroban state size at periodic intervals.
    // Check if already present in live_entries (from transaction delta)
    let has_window_entry = all_live_entries.iter().any(|e| {
        matches!(
            &e.data,
            LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(_))
        )
    });
    if !has_window_entry {
        if let Some(state_size) = soroban_state_size {
            if let Some(window_entry) = compute_soroban_state_size_window_entry(
                header.ledger_seq,
                bucket_list,
                state_size,
                None,
            ) {
                tracing::debug!(
                    ledger_seq = header.ledger_seq,
                    soroban_state_size = state_size,
                    "Added LiveSorobanStateSizeWindow entry to live entries"
                );
                all_live_entries.push(window_entry);
            }
        }
    }

    // Update hot archive FIRST (matches stellar-core order: addHotArchiveBatch before addLiveBatch).
    // IMPORTANT: Must always call add_batch for protocol 23+ even with empty entries,
    // because the hot archive bucket list needs to run spill logic at the same
    // ledger boundaries as the live bucket list.
    {
        let pre_hash = hot_archive_bucket_list.hash();
        tracing::info!(
            ledger_seq = header.ledger_seq,
            pre_hash = %pre_hash,
            archived_count = eviction.archived_entries.len(),
            "Hot archive add_batch - BEFORE"
        );
        // HotArchiveBucketList::add_batch takes (ledger_seq, protocol_version, archived_entries, restored_keys)
        // restored_keys contains entries restored via RestoreFootprint or InvokeHostFunction
        hot_archive_bucket_list
            .add_batch(
                header.ledger_seq,
                header.ledger_version,
                eviction.archived_entries,
                tx_set_result.hot_archive_restored_keys.clone(),
            )
            .map_err(HistoryError::Bucket)?;
        let post_hash = hot_archive_bucket_list.hash();
        tracing::info!(
            ledger_seq = header.ledger_seq,
            post_hash = %post_hash,
            hash_changed = (pre_hash != post_hash),
            "Hot archive add_batch - AFTER"
        );
    }

    // Apply changes to live bucket list SECOND (after hot archive update).
    // Debug: compute hash of entries for comparison with verify-execution
    let init_hash = debug_xdr_hash(&init_entries);
    let live_hash = debug_xdr_hash(&all_live_entries);
    let dead_hash = debug_xdr_hash(&all_dead_entries);
    tracing::info!(
        ledger_seq = header.ledger_seq,
        init_count = init_entries.len(),
        live_count = all_live_entries.len(),
        dead_count = all_dead_entries.len(),
        init_hash = %&init_hash[..16],
        live_hash = %&live_hash[..16],
        dead_hash = %&dead_hash[..16],
        "Replay add_batch entry counts - FINAL"
    );
    bucket_list
        .add_batch(
            header.ledger_seq,
            header.ledger_version,
            BucketListType::Live,
            init_entries.clone(),
            all_live_entries,
            all_dead_entries,
        )
        .map_err(HistoryError::Bucket)?;

    // Debug: log bucket list hash after add_batch for comparison with verify-execution
    let post_add_batch_hash = bucket_list.hash();
    tracing::info!(
        ledger_seq = header.ledger_seq,
        live_bucket_hash = %post_add_batch_hash.to_hex(),
        "Bucket list hash after add_batch"
    );

    if config.verify_bucket_list {
        verify_bucket_list_hash(
            config,
            header,
            bucket_list,
            hot_archive_bucket_list,
            eviction_iterator,
        )?;
    }

    let tx_count = tx_set_result.results.len() as u32;
    let op_count: u32 = tx_set_result
        .results
        .iter()
        .map(|r| r.operation_results.len() as u32)
        .sum();
    let ledger_hash = verify::compute_header_hash(header)?;

    // Compute accurate soroban state size delta from the full change records.
    // This uses the before/after state to accurately track size changes from
    // created, updated, and deleted entries.
    let cost_params = Some((&cpu_cost_params, &mem_cost_params));
    let soroban_state_size_delta =
        compute_soroban_state_size_delta(&changes, header.ledger_version, cost_params);

    Ok(LedgerReplayResult {
        sequence: header.ledger_seq,
        protocol_version: header.ledger_version,
        ledger_hash,
        tx_count,
        op_count,
        fee_pool_delta,
        total_coins_delta,
        init_entries,
        live_entries,
        dead_entries,
        changes,
        eviction_iterator: eviction.updated_iterator,
        soroban_state_size_delta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_bucket::{BucketList, HotArchiveBucketList};
    use henyey_common::NetworkId;
    use stellar_xdr::curr::{Hash, LedgerEntry, LedgerEntryData, LedgerEntryExt};

    use super::super::tests::{make_empty_tx_set, make_test_header};

    fn make_contract_data_entry(seq: u32, key_bytes: &[u8], val_bytes: &[u8]) -> LedgerEntry {
        use stellar_xdr::curr::{
            ContractDataDurability, ContractDataEntry, ContractId, ExtensionPoint, ScAddress, ScVal,
        };
        LedgerEntry {
            last_modified_ledger_seq: seq,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(ContractId(Hash(std::array::from_fn(|i| {
                    key_bytes.get(i).copied().unwrap_or(0)
                })))),
                key: ScVal::Bytes(
                    stellar_xdr::curr::ScBytes::try_from(key_bytes.to_vec()).unwrap(),
                ),
                durability: ContractDataDurability::Persistent,
                val: ScVal::Bytes(
                    stellar_xdr::curr::ScBytes::try_from(val_bytes.to_vec()).unwrap(),
                ),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_account_entry(seq: u32) -> LedgerEntry {
        use stellar_xdr::curr::{
            AccountEntry, AccountEntryExt, AccountId, PublicKey, SequenceNumber, Thresholds,
            Uint256,
        };
        LedgerEntry {
            last_modified_ledger_seq: seq,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
                balance: 1000000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: stellar_xdr::curr::String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: stellar_xdr::curr::VecM::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    #[test]
    fn test_replay_ledger_with_execution_bucket_hash_mismatch() {
        // Use checkpoint ledger (seq % 64 == 63) so bucket list verification runs
        let mut header = make_test_header(127);
        // Use protocol version 25 (P24+) for correct combined hash semantics
        header.ledger_version = 25;
        header.bucket_list_hash = Hash([1u8; 32]);

        let tx_set = TransactionSetVariant::Classic(make_empty_tx_set());
        let mut bucket_list = BucketList::new();
        let mut hot_archive = HotArchiveBucketList::new();

        let config = ReplayConfig {
            verify_results: false,
            verify_bucket_list: true,
            emit_classic_events: false,
            backfill_stellar_asset_events: false,
            run_eviction: true, // Required for P23+ verification
            eviction_settings: StateArchivalSettings::default(),
            wait_for_publish: false,
        };

        // Pass an eviction_iterator for P23+ verification (eviction_running check)
        let eviction_iterator = Some(henyey_bucket::EvictionIterator::new(0));

        let result = replay_ledger_with_execution(
            &header,
            &tx_set,
            ReplayExecutionContext {
                bucket_list: &mut bucket_list,
                hot_archive_bucket_list: &mut hot_archive,
                network_id: &NetworkId::testnet(),
                config: &config,
                expected_tx_results: None,
                eviction_iterator,
                module_cache: None,
                soroban_state_size: None,
                prev_id_pool: 0,
                offer_entries: None,
            },
        );

        assert!(matches!(result, Err(HistoryError::VerificationFailed(_))));
    }

    #[test]
    fn test_replay_ledger_with_execution_tx_set_hash_mismatch() {
        let tx_set = TransactionSetVariant::Classic(make_empty_tx_set());
        let mut header = make_test_header(100);
        header.scp_value.tx_set_hash = Hash([2u8; 32]);

        let mut bucket_list = BucketList::new();
        let mut hot_archive = HotArchiveBucketList::new();
        let config = ReplayConfig {
            verify_results: true,
            verify_bucket_list: false,
            emit_classic_events: false,
            backfill_stellar_asset_events: false,
            run_eviction: false,
            eviction_settings: StateArchivalSettings::default(),
            wait_for_publish: false,
        };

        let result = replay_ledger_with_execution(
            &header,
            &tx_set,
            ReplayExecutionContext {
                bucket_list: &mut bucket_list,
                hot_archive_bucket_list: &mut hot_archive,
                network_id: &NetworkId::testnet(),
                config: &config,
                expected_tx_results: None,
                eviction_iterator: None,
                module_cache: None,
                soroban_state_size: None,
                prev_id_pool: 0,
                offer_entries: None,
            },
        );

        assert!(matches!(result, Err(HistoryError::InvalidTxSetHash { .. })));
    }

    #[test]
    fn test_replay_ledger_with_execution_tx_result_hash_mismatch() {
        let tx_set = TransactionSetVariant::Classic(make_empty_tx_set());
        let tx_set_hash = verify::compute_tx_set_hash(&tx_set).expect("tx set hash");

        let mut header = make_test_header(100);
        header.scp_value.tx_set_hash = Hash(*tx_set_hash.as_bytes());
        header.tx_set_result_hash = Hash([3u8; 32]);

        let mut bucket_list = BucketList::new();
        let mut hot_archive = HotArchiveBucketList::new();
        let config = ReplayConfig {
            verify_results: true,
            verify_bucket_list: false,
            emit_classic_events: false,
            backfill_stellar_asset_events: false,
            run_eviction: false,
            eviction_settings: StateArchivalSettings::default(),
            wait_for_publish: false,
        };

        let result = replay_ledger_with_execution(
            &header,
            &tx_set,
            ReplayExecutionContext {
                bucket_list: &mut bucket_list,
                hot_archive_bucket_list: &mut hot_archive,
                network_id: &NetworkId::testnet(),
                config: &config,
                expected_tx_results: None,
                eviction_iterator: None,
                module_cache: None,
                soroban_state_size: None,
                prev_id_pool: 0,
                offer_entries: None,
            },
        );

        assert!(matches!(result, Err(HistoryError::VerificationFailed(_))));
    }

    #[tokio::test]
    async fn test_compute_soroban_state_size_window_entry_at_sample_boundary() {
        use stellar_xdr::curr::{BucketListType, ConfigSettingEntry, StateArchivalSettings};

        // Create archival settings with sample_period=100, sample_size=5
        let archival = StateArchivalSettings {
            live_soroban_state_size_window_sample_period: 100,
            live_soroban_state_size_window_sample_size: 5,
            ..StateArchivalSettings::default()
        };

        // Create initial window with 5 samples
        let initial_window: stellar_xdr::curr::VecM<u64> =
            vec![1000, 2000, 3000, 4000, 5000].try_into().unwrap();

        // Set up bucket list with required config entries
        let mut bucket_list = BucketList::new();

        // Add StateArchival config
        let archival_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(
                archival.clone(),
            )),
            ext: LedgerEntryExt::V0,
        };
        bucket_list
            .add_batch(
                1,
                25,
                BucketListType::Live,
                vec![],
                vec![archival_entry],
                vec![],
            )
            .expect("add archival");

        // Add LiveSorobanStateSizeWindow config
        let window_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(
                initial_window,
            )),
            ext: LedgerEntryExt::V0,
        };
        bucket_list
            .add_batch(
                2,
                25,
                BucketListType::Live,
                vec![],
                vec![window_entry],
                vec![],
            )
            .expect("add window");

        // Test at sample boundary (seq % 100 == 0) with new state size 6000
        let result =
            compute_soroban_state_size_window_entry(200, &bucket_list, 6000, Some(&archival));
        assert!(
            result.is_some(),
            "Should produce window entry at sample boundary"
        );

        let entry = result.unwrap();
        match entry.data {
            LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(
                window,
            )) => {
                let window_vec: Vec<u64> = window.into();
                // Old window: [1000, 2000, 3000, 4000, 5000]
                // After sample: [2000, 3000, 4000, 5000, 6000]
                assert_eq!(window_vec, vec![2000, 3000, 4000, 5000, 6000]);
            }
            _ => panic!("Expected LiveSorobanStateSizeWindow config entry"),
        }
    }

    #[tokio::test]
    async fn test_compute_soroban_state_size_window_entry_not_at_boundary() {
        use stellar_xdr::curr::{BucketListType, ConfigSettingEntry, StateArchivalSettings};

        // Create archival settings with sample_period=100
        let archival = StateArchivalSettings {
            live_soroban_state_size_window_sample_period: 100,
            live_soroban_state_size_window_sample_size: 5,
            ..StateArchivalSettings::default()
        };

        let initial_window: stellar_xdr::curr::VecM<u64> =
            vec![1000, 2000, 3000, 4000, 5000].try_into().unwrap();

        let mut bucket_list = BucketList::new();

        let archival_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(
                archival.clone(),
            )),
            ext: LedgerEntryExt::V0,
        };
        bucket_list
            .add_batch(
                1,
                25,
                BucketListType::Live,
                vec![],
                vec![archival_entry],
                vec![],
            )
            .expect("add archival");

        let window_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(
                initial_window,
            )),
            ext: LedgerEntryExt::V0,
        };
        bucket_list
            .add_batch(
                2,
                25,
                BucketListType::Live,
                vec![],
                vec![window_entry],
                vec![],
            )
            .expect("add window");

        // Test NOT at sample boundary (seq % 100 != 0)
        let result =
            compute_soroban_state_size_window_entry(201, &bucket_list, 6000, Some(&archival));
        assert!(
            result.is_none(),
            "Should NOT produce window entry when not at sample boundary"
        );
    }

    #[tokio::test]
    async fn test_compute_soroban_state_size_window_entry_resize_smaller() {
        use stellar_xdr::curr::{BucketListType, ConfigSettingEntry, StateArchivalSettings};

        // New settings want size 3 instead of 5
        let archival = StateArchivalSettings {
            live_soroban_state_size_window_sample_period: 100,
            live_soroban_state_size_window_sample_size: 3,
            ..StateArchivalSettings::default()
        };

        // Current window has 5 entries
        let initial_window: stellar_xdr::curr::VecM<u64> =
            vec![1000, 2000, 3000, 4000, 5000].try_into().unwrap();

        let mut bucket_list = BucketList::new();

        let archival_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(
                archival.clone(),
            )),
            ext: LedgerEntryExt::V0,
        };
        bucket_list
            .add_batch(
                1,
                25,
                BucketListType::Live,
                vec![],
                vec![archival_entry],
                vec![],
            )
            .expect("add archival");

        let window_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(
                initial_window,
            )),
            ext: LedgerEntryExt::V0,
        };
        bucket_list
            .add_batch(
                2,
                25,
                BucketListType::Live,
                vec![],
                vec![window_entry],
                vec![],
            )
            .expect("add window");

        // Even when not at sample boundary, resize should trigger update
        let result =
            compute_soroban_state_size_window_entry(201, &bucket_list, 6000, Some(&archival));
        assert!(
            result.is_some(),
            "Should produce window entry when resizing"
        );

        let entry = result.unwrap();
        match entry.data {
            LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(
                window,
            )) => {
                let window_vec: Vec<u64> = window.into();
                // Old: [1000, 2000, 3000, 4000, 5000] -> resized to [3000, 4000, 5000]
                assert_eq!(window_vec, vec![3000, 4000, 5000]);
            }
            _ => panic!("Expected LiveSorobanStateSizeWindow config entry"),
        }
    }

    #[test]
    fn test_soroban_state_size_delta_created_entries() {
        // Test that created ContractData entries add to the delta
        let entry1 = make_contract_data_entry(1, b"key1", b"value1_data");
        let entry2 = make_contract_data_entry(1, b"key2", b"value2_longer_data");

        let changes = vec![
            EntryChange::Created(entry1.clone()),
            EntryChange::Created(entry2.clone()),
        ];

        let delta = compute_soroban_state_size_delta(&changes, 25, None);

        // Delta should be positive (sum of both entry sizes)
        assert!(delta > 0, "Delta should be positive for created entries");

        // Verify it equals the sum of individual entry sizes
        let expected =
            soroban_entry_size(&entry1, 25, None) + soroban_entry_size(&entry2, 25, None);
        assert_eq!(delta, expected);
    }

    #[test]
    fn test_soroban_state_size_delta_updated_entries() {
        // Test that updated entries compute size difference correctly
        let old_entry = make_contract_data_entry(1, b"key1", b"small");
        let new_entry = make_contract_data_entry(2, b"key1", b"much_larger_value_here");

        let changes = vec![EntryChange::Updated {
            previous: old_entry.clone(),
            current: Box::new(new_entry.clone()),
        }];

        let delta = compute_soroban_state_size_delta(&changes, 25, None);

        // Delta should be positive (new > old)
        let old_size = soroban_entry_size(&old_entry, 25, None);
        let new_size = soroban_entry_size(&new_entry, 25, None);
        assert_eq!(delta, new_size - old_size);
        assert!(delta > 0, "Delta should be positive when entry grows");
    }

    #[test]
    fn test_soroban_state_size_delta_updated_entries_shrink() {
        // Test that shrinking entries produce negative delta
        let old_entry = make_contract_data_entry(1, b"key1", b"this_is_a_very_long_value");
        let new_entry = make_contract_data_entry(2, b"key1", b"tiny");

        let changes = vec![EntryChange::Updated {
            previous: old_entry.clone(),
            current: Box::new(new_entry.clone()),
        }];

        let delta = compute_soroban_state_size_delta(&changes, 25, None);

        // Delta should be negative (new < old)
        let old_size = soroban_entry_size(&old_entry, 25, None);
        let new_size = soroban_entry_size(&new_entry, 25, None);
        assert_eq!(delta, new_size - old_size);
        assert!(delta < 0, "Delta should be negative when entry shrinks");
    }

    #[test]
    fn test_soroban_state_size_delta_deleted_entries() {
        // Test that deleted entries subtract from the delta
        let entry = make_contract_data_entry(1, b"key1", b"some_value_to_delete");

        let changes = vec![EntryChange::Deleted {
            previous: entry.clone(),
        }];

        let delta = compute_soroban_state_size_delta(&changes, 25, None);

        // Delta should be negative (size subtracted)
        let expected = -soroban_entry_size(&entry, 25, None);
        assert_eq!(delta, expected);
        assert!(delta < 0, "Delta should be negative for deleted entries");
    }

    #[test]
    fn test_soroban_state_size_delta_ignores_non_soroban_entries() {
        // Test that non-Soroban entries (like Account) are ignored
        let account_entry = make_account_entry(1);
        let contract_entry = make_contract_data_entry(1, b"key", b"value");

        let changes = vec![
            EntryChange::Created(account_entry.clone()),
            EntryChange::Created(contract_entry.clone()),
        ];

        let delta = compute_soroban_state_size_delta(&changes, 25, None);

        // Should only count the ContractData entry, not Account
        let expected = soroban_entry_size(&contract_entry, 25, None);
        assert_eq!(delta, expected);

        // Account entry size should be 0
        assert_eq!(soroban_entry_size(&account_entry, 25, None), 0);
    }

    #[test]
    fn test_soroban_state_size_delta_mixed_operations() {
        // Test a realistic scenario with creates, updates, and deletes
        let created = make_contract_data_entry(1, b"new_key", b"new_value");
        let old_updated = make_contract_data_entry(1, b"upd_key", b"old");
        let new_updated = make_contract_data_entry(2, b"upd_key", b"new_larger");
        let deleted = make_contract_data_entry(1, b"del_key", b"to_delete");

        let changes = vec![
            EntryChange::Created(created.clone()),
            EntryChange::Updated {
                previous: old_updated.clone(),
                current: Box::new(new_updated.clone()),
            },
            EntryChange::Deleted {
                previous: deleted.clone(),
            },
        ];

        let delta = compute_soroban_state_size_delta(&changes, 25, None);

        // Expected: +created + (new_updated - old_updated) - deleted
        let expected = soroban_entry_size(&created, 25, None)
            + (soroban_entry_size(&new_updated, 25, None)
                - soroban_entry_size(&old_updated, 25, None))
            - soroban_entry_size(&deleted, 25, None);

        assert_eq!(delta, expected);
    }

    #[test]
    fn test_soroban_state_size_delta_empty_changes() {
        // Test that empty changes produce zero delta
        let changes: Vec<EntryChange> = vec![];
        let delta = compute_soroban_state_size_delta(&changes, 25, None);
        assert_eq!(delta, 0);
    }
}
