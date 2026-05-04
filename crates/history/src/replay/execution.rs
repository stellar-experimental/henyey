//! Execution-based ledger replay.
//!
//! Re-executes transactions against the current bucket list state to
//! reconstruct ledger entry changes. This is the default approach used
//! during catchup.

use crate::{is_checkpoint_ledger, verify, HistoryError, Result};
use henyey_bucket::{EvictionIterator, EvictionIteratorExt};
use henyey_common::protocol::{
    hot_archive_supported, protocol_version_is_before, protocol_version_starts_from,
    ProtocolVersion,
};
use henyey_common::Hash256;
use henyey_ledger::{
    execution::{
        execute_transaction_set, load_config_setting, require_soroban_config, SorobanContext,
    },
    prepend_fee_event, EntryChange, LedgerDelta, LedgerError, LedgerSnapshot, SnapshotHandle,
    TransactionSetVariant,
};
use henyey_tx::{muxed_to_account_id, soroban::SorobanConfig, LedgerContext, TransactionFrame};
use sha2::{Digest, Sha256};
use stellar_xdr::curr::{
    BucketListType, ConfigSettingEntry, ConfigSettingId, LedgerEntry, LedgerEntryData,
    LedgerEntryExt, LedgerHeader, LedgerKey, LedgerKeyConfigSetting, StateArchivalSettings,
    TransactionResultSet, WriteXdr,
};

use super::diff::log_tx_result_mismatch;
use super::{LedgerReplayResult, ReplayConfig, ReplayExecutionContext};

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
    dead_keys: Vec<LedgerKey>,
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
            dead_keys: Vec::new(),
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
        dead_keys: resolved.evicted_keys(),
        archived_entries: resolved.archived_entries,
        updated_iterator: Some(resolved.end_iterator),
        ran: true,
    })
}

/// Verify the combined bucket list hash at checkpoint boundaries.
///
/// Verification is only reliable at checkpoint boundaries because
/// re-execution without TransactionMeta may produce slightly different entry values.
/// For protocol 23+, eviction must also be running to get accurate results.
fn verify_bucket_list_hash(
    config: &ReplayConfig,
    header: &LedgerHeader,
    bucket_list: &henyey_bucket::BucketList,
    hot_archive_bucket_list: &henyey_bucket::HotArchiveBucketList,
    eviction_iterator: Option<EvictionIterator>,
) -> Result<()> {
    let is_checkpoint = is_checkpoint_ledger(header.ledger_seq);
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
) -> Result<(Vec<LedgerEntry>, Vec<LedgerEntry>, Vec<LedgerKey>)> {
    let delta_init_count = delta_init_entries.len();
    let delta_live_count = live_entries.len();

    // Check if "init" entries already exist in the LIVE bucket list.
    // This can happen when restoring from hot archive when another contract still uses
    // the same ContractCode (shared WASM). Only check ContractCode and ContractData.
    let mut init_entries: Vec<LedgerEntry> = Vec::new();
    let mut moved_to_live_count = 0u32;
    for entry in delta_init_entries {
        let key = henyey_common::entry_to_key(&entry);

        let should_check = henyey_common::is_soroban_entry(&entry);

        let already_in_bucket_list = should_check && bucket_list.get(&key)?.is_some();

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
        if let Some(mut entry) = bucket_list.get(key)? {
            entry.last_modified_ledger_seq = header.ledger_seq;
            live_entries.push(entry);
        }
    }

    // Remove restored entries from dead_entries.
    if !hot_archive_restored_keys.is_empty() {
        let restored_set: std::collections::HashSet<_> = hot_archive_restored_keys.iter().collect();
        dead_entries.retain(|k| !restored_set.contains(k));
    }

    Ok((init_entries, live_entries, dead_entries))
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
) -> Result<Option<LedgerEntry>> {
    use stellar_xdr::curr::VecM;

    // Load StateArchival settings
    let archival = if let Some(override_settings) = archival_override {
        override_settings.clone()
    } else {
        let archival_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::StateArchival,
        });
        let archival_entry = bucket_list.get(&archival_key)?.ok_or_else(|| {
            HistoryError::VerificationFailed(
                "compute_soroban_state_size_window_entry: StateArchival config entry not found"
                    .into(),
            )
        })?;
        match archival_entry.data {
            LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(settings)) => settings,
            _ => {
                return Err(HistoryError::VerificationFailed(
                    "compute_soroban_state_size_window_entry: expected StateArchival, got wrong variant".into(),
                ));
            }
        }
    };

    let sample_period = archival.live_soroban_state_size_window_sample_period;
    let sample_size = archival.live_soroban_state_size_window_sample_size as usize;
    if sample_period == 0 || sample_size == 0 {
        return Err(HistoryError::VerificationFailed(
            "compute_soroban_state_size_window_entry: sample_period or sample_size is 0 (invalid config)".into(),
        ));
    }

    // Load current window
    let window_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: ConfigSettingId::LiveSorobanStateSizeWindow,
    });
    let window_entry = bucket_list.get(&window_key)?.ok_or_else(|| {
        HistoryError::VerificationFailed(
            "compute_soroban_state_size_window_entry: LiveSorobanStateSizeWindow config entry not found".into(),
        )
    })?;
    let window = match window_entry.data {
        LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(w)) => w,
        _ => {
            return Err(HistoryError::VerificationFailed(
                "compute_soroban_state_size_window_entry: expected LiveSorobanStateSizeWindow, got wrong variant".into(),
            ));
        }
    };

    let mut window_vec: Vec<u64> = window.into();
    if window_vec.is_empty() {
        return Err(HistoryError::VerificationFailed(
            "compute_soroban_state_size_window_entry: window vec is empty (invariant violation)"
                .into(),
        ));
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
        return Ok(None);
    }

    let window_vecm: VecM<u64> = window_vec.try_into()?;

    Ok(Some(LedgerEntry {
        last_modified_ledger_seq: seq,
        data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(
            window_vecm,
        )),
        ext: LedgerEntryExt::V0,
    }))
}

fn combined_bucket_list_hash(
    live_bucket_list: &henyey_bucket::BucketList,
    hot_archive_bucket_list: &henyey_bucket::HotArchiveBucketList,
    protocol_version: u32,
) -> Hash256 {
    let live_hash = live_bucket_list.hash();
    if hot_archive_supported(protocol_version) {
        let hot_hash = hot_archive_bucket_list.hash();
        tracing::info!(
            live_hash = %live_hash,
            hot_archive_hash = %hot_hash,
            "Computing combined bucket list hash (hot archive supported)"
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
            "Using live bucket list hash only (hot archive not supported)"
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
    bucket_list.resolve_all_pending_merges()?;

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
    // Load SorobanConfig from ledger ConfigSettingEntry for accurate Soroban execution.
    // Only loaded for protocol >= 20 (Soroban protocol) with non-empty tx sets,
    // matching stellar-core's guard in LedgerManagerImpl which only calls
    // loadFromLedger for Soroban protocol versions. With an empty tx set, config
    // settings may not exist (e.g., empty bucket list in tests).
    let soroban_config =
        if protocol_version_starts_from(header.ledger_version, ProtocolVersion::V20)
            && !transactions.is_empty()
        {
            require_soroban_config(&snapshot, header.ledger_version)?
        } else {
            SorobanConfig::default()
        };
    // Save cost params before soroban_config is moved into execute_transaction_set
    let cpu_cost_params = soroban_config.cpu_cost_params.clone();
    let mem_cost_params = soroban_config.mem_cost_params.clone();
    let eviction_settings = match load_config_setting(&snapshot, ConfigSettingId::StateArchival) {
        Ok(Some(ConfigSettingEntry::StateArchival(settings))) => settings,
        Ok(Some(_)) => {
            return Err(HistoryError::VerificationFailed(
                "replay: unexpected ConfigSettingEntry variant for StateArchival key".to_string(),
            ));
        }
        Ok(None) => config.eviction_settings.clone(),
        Err(e) => {
            return Err(HistoryError::VerificationFailed(format!(
                "replay: failed to load StateArchival config: {e}"
            )));
        }
    };
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

    // Add fee events to transaction metadata (matching online mode behavior).
    // Use pre-refund fee (fee_charged + fee_refund) to match stellar-core's
    // behavior: the fee event records the full debit before refund.
    if classic_events.events_enabled(header.ledger_version) {
        for (idx, ((envelope, _), meta)) in transactions
            .iter()
            .zip(tx_set_result.tx_result_metas.iter_mut())
            .enumerate()
        {
            if idx >= tx_set_result.results.len() {
                break;
            }
            let result = &tx_set_result.results[idx];
            let fee_charged = result.pre_refund_fee();
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
    )?;

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
    all_dead_entries.extend(eviction.dead_keys);

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
    // Always compute the window entry on sample ledgers, even if a config upgrade
    // already placed a resized window in live_entries. The compute function handles
    // both resize and shift+push; if a config upgrade resized the window, we replace
    // the resize-only entry with the resize+shift+push result.
    if let Some(state_size) = soroban_state_size {
        if let Some(window_entry) = compute_soroban_state_size_window_entry(
            header.ledger_seq,
            bucket_list,
            state_size,
            Some(&eviction_settings),
        )? {
            // Remove any existing window entry (e.g. from a config upgrade resize)
            all_live_entries.retain(|e| {
                !matches!(
                    &e.data,
                    LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(
                        _
                    ))
                )
            });
            tracing::debug!(
                ledger_seq = header.ledger_seq,
                soroban_state_size = state_size,
                "Added LiveSorobanStateSizeWindow entry to live entries"
            );
            all_live_entries.push(window_entry);
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
        make_account_entry_with_id(seq, 0)
    }

    /// Build an `Account` entry whose `AccountId` is uniquely determined by
    /// `account_id_byte`. Used to construct multiple distinct `LedgerKey`s in
    /// tests that need more than one Account in the bucket list.
    fn make_account_entry_with_id(seq: u32, account_id_byte: u8) -> LedgerEntry {
        use stellar_xdr::curr::{
            AccountEntry, AccountEntryExt, AccountId, PublicKey, SequenceNumber, Thresholds,
            Uint256,
        };
        LedgerEntry {
            last_modified_ledger_seq: seq,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
                    [account_id_byte; 32],
                ))),
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

    /// Build a `ContractCode` entry with a deterministic 32-byte hash derived
    /// from `hash_byte` and a fixed 100-byte WASM payload. Mirrors the helper
    /// in `crates/tx/src/operations/execute/restore_footprint.rs`.
    fn make_contract_code_entry(seq: u32, hash_byte: u8) -> LedgerEntry {
        use stellar_xdr::curr::{ContractCodeEntry, ContractCodeEntryExt};
        LedgerEntry {
            last_modified_ledger_seq: seq,
            data: LedgerEntryData::ContractCode(ContractCodeEntry {
                ext: ContractCodeEntryExt::V0,
                hash: Hash([hash_byte; 32]),
                code: vec![0u8; 100].try_into().unwrap(),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    /// Protocol version used by all `classify_delta_entries` tests.
    const CLASSIFY_TEST_PROTOCOL: u32 = 25;

    /// Seed `bucket_list` with the given live entries at `ledger_seq` and
    /// protocol [`CLASSIFY_TEST_PROTOCOL`]. Panics on failure (test-only).
    fn seed_bucket_list_live(
        bucket_list: &mut BucketList,
        ledger_seq: u32,
        live: Vec<LedgerEntry>,
    ) {
        use stellar_xdr::curr::BucketListType;
        bucket_list
            .add_batch(
                ledger_seq,
                CLASSIFY_TEST_PROTOCOL,
                BucketListType::Live,
                vec![],
                live,
                vec![],
            )
            .expect("seed bucket list");
    }

    // ---- classify_delta_entries unit tests (issue #2206) ---------------------
    //
    // These tests pin down the three behaviors named in #2206:
    //   1. Soroban INIT entries already in the bucket list are reclassified
    //      as LIVE.
    //   2. Non-Soroban INIT entries skip the bucket-list check entirely.
    //   3. Soroban INIT entries not in the bucket list remain as INIT.
    //
    // Plus parity-safe sanity coverage of the same function: mixed
    // Soroban/non-Soroban batches, hot-archive restored-key reconciliation,
    // and the empty-input case.

    #[test]
    fn test_classify_delta_entries_soroban_init_already_in_bucket_list_moved_to_live() {
        // Bucket list has a ContractData entry at key K.
        let existing = make_contract_data_entry(1, &[0xAA], &[0x01]);
        let mut bucket_list = BucketList::new();
        seed_bucket_list_live(&mut bucket_list, 1, vec![existing]);

        // Delta INIT contains the same key K but with a *different* value.
        // This proves the gate uses key equality, not entry equality.
        let delta_init = make_contract_data_entry(2, &[0xAA], &[0x02]);
        let header = make_test_header(100);

        let (init, live, dead) = classify_delta_entries(
            &header,
            &bucket_list,
            vec![delta_init.clone()],
            vec![],
            vec![],
            &[],
        )
        .unwrap();

        assert!(init.is_empty(), "INIT entry should have been moved to LIVE");
        assert_eq!(live.len(), 1, "exactly one entry should be in LIVE");
        // The *delta* entry is moved (with its new value), not the
        // bucket-list entry.
        assert_eq!(live[0].data, delta_init.data);
        assert!(dead.is_empty());
    }

    #[test]
    fn test_classify_delta_entries_soroban_init_not_in_bucket_list_stays_init() {
        let bucket_list = BucketList::new();
        let delta_init = make_contract_data_entry(2, &[0xBB], &[0x10]);
        let header = make_test_header(100);

        let (init, live, dead) = classify_delta_entries(
            &header,
            &bucket_list,
            vec![delta_init.clone()],
            vec![],
            vec![],
            &[],
        )
        .unwrap();

        assert_eq!(init.len(), 1);
        assert_eq!(init[0].data, delta_init.data);
        assert!(live.is_empty());
        assert!(dead.is_empty());
    }

    #[test]
    fn test_classify_delta_entries_contract_code_init_already_in_bucket_list_moved_to_live() {
        // Same as the ContractData case but with ContractCode — the second
        // arm of the inline `matches!` gate. Catches a refactor that drops
        // either arm independently.
        let existing = make_contract_code_entry(1, 0xCC);
        let mut bucket_list = BucketList::new();
        seed_bucket_list_live(&mut bucket_list, 1, vec![existing]);

        let delta_init = make_contract_code_entry(2, 0xCC);
        let header = make_test_header(100);

        let (init, live, dead) = classify_delta_entries(
            &header,
            &bucket_list,
            vec![delta_init.clone()],
            vec![],
            vec![],
            &[],
        )
        .unwrap();

        assert!(init.is_empty());
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].data, delta_init.data);
        assert!(dead.is_empty());
    }

    #[test]
    fn test_classify_delta_entries_non_soroban_init_skips_bucket_list_check() {
        // Direct regression test for the gap #2206 names: the `should_check`
        // gate at execution.rs:237-241 must short-circuit non-Soroban entries
        // before the bucket-list lookup. If a refactor drops the gate (e.g.
        // replaces it with an unconditional `bucket_list.get(...).is_some()`),
        // this test fails.
        let existing_account = make_account_entry_with_id(1, 0x01);
        let mut bucket_list = BucketList::new();
        seed_bucket_list_live(&mut bucket_list, 1, vec![existing_account.clone()]);

        let delta_init = make_account_entry_with_id(2, 0x01);
        let header = make_test_header(100);

        let (init, live, dead) = classify_delta_entries(
            &header,
            &bucket_list,
            vec![delta_init.clone()],
            vec![],
            vec![],
            &[],
        )
        .unwrap();

        assert_eq!(
            init.len(),
            1,
            "non-Soroban INIT entry must NOT be moved to LIVE even when its \
             key is already in the bucket list"
        );
        assert_eq!(init[0].data, delta_init.data);
        assert!(live.is_empty());
        assert!(dead.is_empty());
    }

    #[test]
    fn test_classify_delta_entries_mixed_soroban_and_non_soroban_init() {
        // Bucket list contains:
        //   - K1 = ContractData (Soroban)
        //   - K2 = Account (non-Soroban)
        let existing_cd = make_contract_data_entry(1, &[0xD1], &[0x01]);
        let existing_acct = make_account_entry_with_id(1, 0x02);
        let mut bucket_list = BucketList::new();
        seed_bucket_list_live(
            &mut bucket_list,
            1,
            vec![existing_cd.clone(), existing_acct.clone()],
        );

        // Delta INIT contains:
        //   - ContractData at K1 (Soroban + already in bucket list → LIVE)
        //   - ContractCode at K3 (Soroban + new → INIT)
        //   - Account at K2 (non-Soroban + already in bucket list → INIT,
        //                     gate skipped)
        let delta_cd = make_contract_data_entry(2, &[0xD1], &[0x02]);
        let delta_cc = make_contract_code_entry(2, 0xD3);
        let delta_acct = make_account_entry_with_id(2, 0x02);
        let header = make_test_header(100);

        let (init, live, dead) = classify_delta_entries(
            &header,
            &bucket_list,
            vec![delta_cd.clone(), delta_cc.clone(), delta_acct.clone()],
            vec![],
            vec![],
            &[],
        )
        .unwrap();

        // INIT: ContractCode (new) and Account (gate skipped) — order
        // preserved from input minus the moved entries.
        assert_eq!(init.len(), 2);
        let init_data: Vec<_> = init.iter().map(|e| e.data.clone()).collect();
        assert!(init_data.contains(&delta_cc.data));
        assert!(init_data.contains(&delta_acct.data));

        // LIVE: ContractData only.
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].data, delta_cd.data);

        assert!(dead.is_empty());
    }

    #[test]
    fn test_classify_delta_entries_hot_archive_restored_adds_to_live_and_removes_from_dead() {
        // Bucket list has a ContractData entry at K (last_modified_ledger_seq=1).
        let existing = make_contract_data_entry(1, &[0xE1], &[0x01]);
        let restored_key = henyey_common::entry_to_key(&existing);

        // K_other is a different ContractData key that should be preserved
        // in the returned dead_entries.
        let other_entry = make_contract_data_entry(1, &[0xE2], &[0x02]);
        let other_key = henyey_common::entry_to_key(&other_entry);

        let mut bucket_list = BucketList::new();
        seed_bucket_list_live(&mut bucket_list, 1, vec![existing.clone()]);

        let header = make_test_header(100);

        let (init, live, dead) = classify_delta_entries(
            &header,
            &bucket_list,
            vec![],
            vec![],
            vec![restored_key.clone(), other_key.clone()],
            std::slice::from_ref(&restored_key),
        )
        .unwrap();

        assert!(init.is_empty());

        // LIVE contains the bucket-list entry with last_modified_ledger_seq
        // rewritten to header.ledger_seq.
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].data, existing.data);
        assert_eq!(live[0].last_modified_ledger_seq, header.ledger_seq);

        // DEAD: K removed; K_other preserved.
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0], other_key);
    }

    #[test]
    fn test_classify_delta_entries_hot_archive_restored_missing_from_bucket_list_is_noop() {
        // Empty bucket list — bucket_list.get(key) returns Ok(None), and the
        // restored-key loop silently skips. Pins down the no-op behavior so
        // a future refactor doesn't accidentally turn it into an error or a
        // panic.
        let bucket_list = BucketList::new();
        let header = make_test_header(100);

        // Construct a key that is not in the bucket list.
        let absent_entry = make_contract_data_entry(1, &[0xF1], &[0x01]);
        let absent_key = henyey_common::entry_to_key(&absent_entry);

        let (init, live, dead) =
            classify_delta_entries(&header, &bucket_list, vec![], vec![], vec![], &[absent_key])
                .unwrap();

        assert!(init.is_empty());
        assert!(live.is_empty());
        assert!(dead.is_empty());
    }

    #[test]
    fn test_classify_delta_entries_empty_inputs() {
        let bucket_list = BucketList::new();
        let header = make_test_header(100);

        let (init, live, dead) =
            classify_delta_entries(&header, &bucket_list, vec![], vec![], vec![], &[]).unwrap();

        assert!(init.is_empty());
        assert!(live.is_empty());
        assert!(dead.is_empty());
    }

    #[test]
    fn test_replay_ledger_with_execution_bucket_hash_mismatch() {
        // Use checkpoint ledger (is_checkpoint_ledger(127) == true) so bucket list verification runs
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
            verify_header_hash: true,
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
            verify_header_hash: false,
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
            verify_header_hash: false,
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

        assert!(matches!(
            result,
            Err(HistoryError::VerificationHashMismatch(_))
        ));
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
            compute_soroban_state_size_window_entry(200, &bucket_list, 6000, Some(&archival))
                .expect("should not error");
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
            compute_soroban_state_size_window_entry(201, &bucket_list, 6000, Some(&archival))
                .expect("should not error");
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
            compute_soroban_state_size_window_entry(201, &bucket_list, 6000, Some(&archival))
                .expect("should not error");
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

    #[tokio::test]
    async fn test_compute_soroban_state_size_window_entry_zero_sample_period_errors() {
        use stellar_xdr::curr::StateArchivalSettings;

        let archival = StateArchivalSettings {
            live_soroban_state_size_window_sample_period: 0,
            live_soroban_state_size_window_sample_size: 5,
            ..StateArchivalSettings::default()
        };

        let bucket_list = BucketList::new();
        let result =
            compute_soroban_state_size_window_entry(100, &bucket_list, 5000, Some(&archival));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("sample_period or sample_size is 0"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_compute_soroban_state_size_window_entry_missing_window_errors() {
        use stellar_xdr::curr::StateArchivalSettings;

        let archival = StateArchivalSettings {
            live_soroban_state_size_window_sample_period: 100,
            live_soroban_state_size_window_sample_size: 5,
            ..StateArchivalSettings::default()
        };

        // Empty bucket list — no window entry exists
        let bucket_list = BucketList::new();
        let result =
            compute_soroban_state_size_window_entry(100, &bucket_list, 5000, Some(&archival));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_compute_soroban_state_size_window_entry_empty_window_errors() {
        use stellar_xdr::curr::{BucketListType, ConfigSettingEntry, StateArchivalSettings};

        let archival = StateArchivalSettings {
            live_soroban_state_size_window_sample_period: 100,
            live_soroban_state_size_window_sample_size: 5,
            ..StateArchivalSettings::default()
        };

        // Add an empty window to the bucket list
        let mut bucket_list = BucketList::new();
        let empty_window: stellar_xdr::curr::VecM<u64> = vec![].try_into().unwrap();
        let window_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::LiveSorobanStateSizeWindow(
                empty_window,
            )),
            ext: LedgerEntryExt::V0,
        };
        bucket_list
            .add_batch(
                1,
                25,
                BucketListType::Live,
                vec![],
                vec![window_entry],
                vec![],
            )
            .expect("add window");

        let result =
            compute_soroban_state_size_window_entry(100, &bucket_list, 5000, Some(&archival));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("empty"), "unexpected error: {err}");
    }

    /// Regression test for #2226: loading StateArchival with a wrong
    /// ConfigSettingEntry variant must produce a fatal VerificationFailed error,
    /// not silently fall back to a default.
    #[test]
    fn test_load_state_archival_wrong_variant_errors() {
        use henyey_ledger::execution::load_config_setting;
        use stellar_xdr::curr::{
            ConfigSettingContractComputeV0, ConfigSettingEntry, ConfigSettingId, LedgerEntry,
            LedgerEntryData, LedgerEntryExt, LedgerKey, LedgerKeyConfigSetting,
        };

        let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::StateArchival,
        });
        // Store a wrong variant (ContractComputeV0) at the StateArchival key
        let wrong_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::ContractComputeV0(
                ConfigSettingContractComputeV0 {
                    ledger_max_instructions: 0,
                    tx_max_instructions: 0,
                    fee_rate_per_instructions_increment: 0,
                    tx_memory_limit: 0,
                },
            )),
            ext: LedgerEntryExt::V0,
        };

        let mut entries = std::collections::HashMap::new();
        entries.insert(key, wrong_entry);
        let snapshot = henyey_ledger::LedgerSnapshot::new(
            LedgerHeader::default(),
            henyey_common::Hash256::ZERO,
            entries,
        );
        let handle = henyey_ledger::SnapshotHandle::new(snapshot);

        // Exercise the exact match pattern used at the replay call site
        let result: crate::Result<StateArchivalSettings> =
            match load_config_setting(&handle, ConfigSettingId::StateArchival) {
                Ok(Some(ConfigSettingEntry::StateArchival(settings))) => Ok(settings),
                Ok(Some(_)) => Err(HistoryError::VerificationFailed(
                    "replay: unexpected ConfigSettingEntry variant for StateArchival key"
                        .to_string(),
                )),
                Ok(None) => Ok(StateArchivalSettings::default()),
                Err(e) => Err(HistoryError::VerificationFailed(format!(
                    "replay: failed to load StateArchival config: {e}"
                ))),
            };

        assert!(result.is_err(), "wrong variant should produce an error");
        let err = result.unwrap_err();
        assert!(
            err.is_fatal_catchup_failure(),
            "wrong variant error should be fatal"
        );
        assert!(
            err.to_string()
                .contains("unexpected ConfigSettingEntry variant"),
            "unexpected error message: {err}"
        );
    }

    /// Regression test for #2226: I/O error during config lookup must produce
    /// a fatal VerificationFailed error, not be silently masked.
    #[test]
    fn test_load_state_archival_io_error_propagates() {
        use henyey_ledger::execution::load_config_setting;
        use std::sync::Arc;
        use stellar_xdr::curr::{ConfigSettingEntry, ConfigSettingId};

        let snapshot = henyey_ledger::LedgerSnapshot::empty(1);
        let lookup_fn: henyey_ledger::EntryLookupFn = Arc::new(|_key| {
            Err(henyey_ledger::LedgerError::Snapshot(
                "simulated I/O error".to_string(),
            ))
        });
        let handle = henyey_ledger::SnapshotHandle::with_lookup(snapshot, lookup_fn);

        // Exercise the exact match pattern used at the replay call site
        let result: crate::Result<StateArchivalSettings> =
            match load_config_setting(&handle, ConfigSettingId::StateArchival) {
                Ok(Some(ConfigSettingEntry::StateArchival(settings))) => Ok(settings),
                Ok(Some(_)) => Err(HistoryError::VerificationFailed(
                    "replay: unexpected ConfigSettingEntry variant for StateArchival key"
                        .to_string(),
                )),
                Ok(None) => Ok(StateArchivalSettings::default()),
                Err(e) => Err(HistoryError::VerificationFailed(format!(
                    "replay: failed to load StateArchival config: {e}"
                ))),
            };

        assert!(result.is_err(), "I/O error should produce an error");
        let err = result.unwrap_err();
        assert!(
            err.is_fatal_catchup_failure(),
            "I/O error should be fatal in replay context"
        );
        assert!(
            err.to_string().contains("simulated I/O error"),
            "unexpected error message: {err}"
        );
    }

    /// Regression test: replay path exercises prepend_fee_event with emit_classic_events enabled.
    ///
    /// Verifies that replay_ledger_with_execution succeeds (no panic) when processing
    /// a Soroban ExtendFootprintTtl transaction with emit_classic_events: true.
    /// Both the online (close_ledger) and replay paths share the same prepend_fee_event
    /// logic; this test ensures the replay code path doesn't diverge.
    #[tokio::test]
    async fn test_replay_fee_event_code_path_executes() {
        use henyey_common::NetworkId;
        use henyey_crypto::{sign_hash, SecretKey};
        use stellar_xdr::curr::{
            AccountEntry, AccountEntryExt, AccountId, BucketListType, ContractCodeEntry,
            ContractCodeEntryExt, DecoratedSignature, ExtendFootprintTtlOp, ExtensionPoint,
            LedgerFootprint, LedgerKeyContractCode, Memo, MuxedAccount, Operation, OperationBody,
            Preconditions, SequenceNumber, Signature as XdrSignature, SignatureHint,
            SorobanResources, SorobanTransactionData, SorobanTransactionDataExt, Thresholds,
            Transaction, TransactionEnvelope, TransactionExt, TransactionSet,
            TransactionV1Envelope, TtlEntry, Uint256, VecM,
        };

        let network_id = NetworkId::testnet();
        let secret = SecretKey::from_seed(&[1u8; 32]);
        let source_id = AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256(
            *secret.public_key().as_bytes(),
        )));

        // Build bucket list with soroban config + required entries
        let mut bucket_list = henyey_ledger::new_bucket_list_with_soroban_config();
        let source_entry = LedgerEntry {
            last_modified_ledger_seq: 0,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: source_id.clone(),
                balance: 20_000_000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: Default::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: VecM::default(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        let code_hash = Hash([9u8; 32]);
        let contract_code_entry = LedgerEntry {
            last_modified_ledger_seq: 0,
            data: LedgerEntryData::ContractCode(ContractCodeEntry {
                ext: ContractCodeEntryExt::V0,
                hash: code_hash.clone(),
                code: stellar_xdr::curr::BytesM::try_from(vec![1u8, 2u8, 3u8]).unwrap(),
            }),
            ext: LedgerEntryExt::V0,
        };

        let contract_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: code_hash.clone(),
        });
        let key_hash: Hash = henyey_common::Hash256::hash_xdr(&contract_key).into();
        let ttl_entry = LedgerEntry {
            last_modified_ledger_seq: 0,
            data: LedgerEntryData::Ttl(TtlEntry {
                key_hash,
                live_until_ledger_seq: 10,
            }),
            ext: LedgerEntryExt::V0,
        };

        bucket_list
            .add_batch(
                1,
                25,
                BucketListType::Live,
                vec![source_entry, contract_code_entry, ttl_entry],
                vec![],
                vec![],
            )
            .expect("add_batch");

        // Build Soroban ExtendFootprintTtl transaction
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![contract_key].try_into().unwrap(),
                    read_write: VecM::default(),
                },
                instructions: 0,
                disk_read_bytes: 100,
                write_bytes: 0,
            },
            resource_fee: 100_000,
        };

        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes())),
            fee: 110_000,
            seq_num: SequenceNumber(2),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::ExtendFootprintTtl(ExtendFootprintTtlOp {
                    ext: ExtensionPoint::V0,
                    extend_to: 100,
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V1(soroban_data),
        };

        let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });
        // Sign the transaction
        let frame =
            henyey_tx::TransactionFrame::from_owned_with_network(envelope.clone(), network_id);
        let hash = frame.hash(&network_id).expect("tx hash");
        let signature = sign_hash(&secret, &hash);
        let public_key = secret.public_key();
        let pk_bytes = public_key.as_bytes();
        let hint = SignatureHint([pk_bytes[28], pk_bytes[29], pk_bytes[30], pk_bytes[31]]);
        let decorated = DecoratedSignature {
            hint,
            signature: XdrSignature(signature.0.to_vec().try_into().unwrap()),
        };
        if let TransactionEnvelope::Tx(ref mut env) = envelope {
            env.signatures = vec![decorated].try_into().unwrap();
        }

        // Set up replay context with emit_classic_events: true
        let mut header = make_test_header(2);
        header.ledger_version = 25;

        let tx_set = TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash([0u8; 32]),
            txs: vec![envelope].try_into().unwrap(),
        });

        let config = ReplayConfig {
            verify_results: false,
            verify_bucket_list: false,
            verify_header_hash: false,
            emit_classic_events: true,
            backfill_stellar_asset_events: false,
            run_eviction: false,
            eviction_settings: StateArchivalSettings::default(),
            wait_for_publish: false,
        };

        let mut hot_archive = HotArchiveBucketList::new();

        let result = replay_ledger_with_execution(
            &header,
            &tx_set,
            ReplayExecutionContext {
                bucket_list: &mut bucket_list,
                hot_archive_bucket_list: &mut hot_archive,
                network_id: &network_id,
                config: &config,
                expected_tx_results: None,
                eviction_iterator: None,
                module_cache: None,
                soroban_state_size: None,
                prev_id_pool: 0,
                offer_entries: None,
            },
        );

        // The replay should succeed without panic — the fee event code path is exercised
        assert!(
            result.is_ok(),
            "replay_ledger_with_execution should succeed with emit_classic_events: true, got: {:?}",
            result.err()
        );
    }

    /// Verify that `replay_ledger_with_execution()` correctly runs eviction for
    /// protocol 25: expired entries are removed from the live bucket list,
    /// persistent entries are archived to the hot archive, non-expired entries
    /// survive, and the EvictionIterator config entry is persisted.
    ///
    /// Parity: stellar-core `populateEvictedEntries` (LedgerCloseMetaFrame.cpp:170-187),
    /// `resolveBackgroundEvictionScan` (LedgerManagerImpl.cpp:2848-2851),
    /// eviction scan + resolve (BucketManager.cpp:1231-1265).
    #[test]
    fn test_replay_eviction_application_mixed_temp_persistent() {
        // add_batch may trigger spawn_blocking for async merges
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            test_replay_eviction_application_mixed_temp_persistent_impl();
        });
    }

    fn test_replay_eviction_application_mixed_temp_persistent_impl() {
        use henyey_bucket::EvictionIterator;
        use henyey_common::xdr_to_bytes;
        use sha2::{Digest, Sha256};
        use stellar_xdr::curr::{
            ConfigSettingEntry, ContractCostParamEntry, ContractCostParams, ContractDataDurability,
            ContractDataEntry, ContractId, ExtensionPoint, LedgerKeyConfigSetting,
            LedgerKeyContractData, LedgerKeyTtl, ScAddress, ScBytes, ScVal, StateArchivalSettings,
            TtlEntry,
        };

        let contract = ScAddress::Contract(ContractId(Hash([1u8; 32])));

        // --- Helper: build a contract data entry and its key ---
        let make_data_entry =
            |key_byte: u8, durability: ContractDataDurability| -> (LedgerEntry, LedgerKey) {
                let key = LedgerKey::ContractData(LedgerKeyContractData {
                    contract: contract.clone(),
                    key: ScVal::Bytes(ScBytes(vec![key_byte].try_into().unwrap())),
                    durability,
                });
                let entry = LedgerEntry {
                    last_modified_ledger_seq: 1,
                    data: LedgerEntryData::ContractData(ContractDataEntry {
                        ext: ExtensionPoint::V0,
                        contract: contract.clone(),
                        key: ScVal::Bytes(ScBytes(vec![key_byte].try_into().unwrap())),
                        durability,
                        val: ScVal::I32(42),
                    }),
                    ext: LedgerEntryExt::V0,
                };
                (entry, key)
            };

        // --- Helper: build TTL key and entry for a given data key ---
        let ttl_key_for = |data_key: &LedgerKey| -> LedgerKey {
            let key_bytes = xdr_to_bytes(data_key);
            let hash_bytes: [u8; 32] = Sha256::digest(&key_bytes).into();
            LedgerKey::Ttl(LedgerKeyTtl {
                key_hash: Hash(hash_bytes),
            })
        };

        let make_ttl_entry = |data_key: &LedgerKey, live_until: u32| -> LedgerEntry {
            let key_bytes = xdr_to_bytes(data_key);
            let hash_bytes: [u8; 32] = Sha256::digest(&key_bytes).into();
            LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::Ttl(TtlEntry {
                    key_hash: Hash(hash_bytes),
                    live_until_ledger_seq: live_until,
                }),
                ext: LedgerEntryExt::V0,
            }
        };

        // --- Build entries ---
        // A: Temporary, expired (live_until=99 < ledger_seq=100)
        let (entry_a, key_a) = make_data_entry(0x01, ContractDataDurability::Temporary);
        // B: Persistent, expired
        let (entry_b, key_b) = make_data_entry(0x01, ContractDataDurability::Persistent);
        // C: Temporary, expired
        let (entry_c, key_c) = make_data_entry(0x02, ContractDataDurability::Temporary);
        // D: Persistent, expired
        let (entry_d, key_d) = make_data_entry(0x02, ContractDataDurability::Persistent);
        // E: Persistent, NOT expired (live_until=200 >= ledger_seq=100)
        let (entry_e, key_e) = make_data_entry(0x03, ContractDataDurability::Persistent);
        // F: Temporary, NOT expired (live_until=200 >= ledger_seq=100) — control
        let (entry_f, key_f) = make_data_entry(0x03, ContractDataDurability::Temporary);

        let ttl_a = make_ttl_entry(&key_a, 99);
        let ttl_b = make_ttl_entry(&key_b, 99);
        let ttl_c = make_ttl_entry(&key_c, 99);
        let ttl_d = make_ttl_entry(&key_d, 99);
        let ttl_e = make_ttl_entry(&key_e, 200);
        let ttl_f = make_ttl_entry(&key_f, 200);

        // --- Config entries needed for replay ---
        let cost_param = ContractCostParamEntry {
            ext: ExtensionPoint::V0,
            const_term: 0,
            linear_term: 0,
        };
        let cost_params = ContractCostParams(vec![cost_param].try_into().unwrap());

        let make_config = |setting: ConfigSettingEntry| LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ConfigSetting(setting),
            ext: LedgerEntryExt::V0,
        };

        let config_entries = vec![
            make_config(ConfigSettingEntry::StateArchival(StateArchivalSettings {
                max_entry_ttl: 1_054_080,
                min_persistent_ttl: 4_096,
                min_temporary_ttl: 16,
                persistent_rent_rate_denominator: 252_480,
                temp_rent_rate_denominator: 2_524_800,
                max_entries_to_archive: 100,
                live_soroban_state_size_window_sample_size: 30,
                live_soroban_state_size_window_sample_period: 64,
                eviction_scan_size: 100_000,
                starting_eviction_scan_level: 0,
            })),
            make_config(ConfigSettingEntry::EvictionIterator(EvictionIterator {
                bucket_list_level: 0,
                is_curr_bucket: true,
                bucket_file_offset: 0,
            })),
            make_config(ConfigSettingEntry::ContractCostParamsCpuInstructions(
                cost_params.clone(),
            )),
            make_config(ConfigSettingEntry::ContractCostParamsMemoryBytes(
                cost_params,
            )),
        ];

        // --- Seed the bucket list ---
        let all_entries: Vec<LedgerEntry> = config_entries
            .into_iter()
            .chain(vec![
                entry_a, entry_b, entry_c, entry_d, entry_e, entry_f, ttl_a, ttl_b, ttl_c, ttl_d,
                ttl_e, ttl_f,
            ])
            .collect();

        let mut bucket_list = BucketList::new();
        bucket_list
            .add_batch(
                1,
                25,
                stellar_xdr::curr::BucketListType::Live,
                all_entries,
                vec![],
                vec![],
            )
            .expect("seed bucket list");

        let mut hot_archive = HotArchiveBucketList::new();

        // --- Build replay header and config ---
        let mut header = make_test_header(100);
        header.ledger_version = 25;

        let tx_set = TransactionSetVariant::Classic(make_empty_tx_set());

        let config = ReplayConfig {
            verify_results: false,
            verify_bucket_list: false,
            verify_header_hash: false,
            emit_classic_events: false,
            backfill_stellar_asset_events: false,
            run_eviction: true,
            eviction_settings: StateArchivalSettings::default(),
            wait_for_publish: false,
        };

        let eviction_iterator = Some(EvictionIterator::new(0));

        // --- Execute replay ---
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
        )
        .expect("replay_ledger_with_execution should succeed");

        // --- Assertion 1: Evicted entries absent from live bucket list ---
        assert!(
            bucket_list.get(&key_a).unwrap().is_none(),
            "entry A (temp, expired) should be evicted from live bucket list"
        );
        assert!(
            bucket_list.get(&ttl_key_for(&key_a)).unwrap().is_none(),
            "TTL for entry A should be evicted from live bucket list"
        );
        assert!(
            bucket_list.get(&key_b).unwrap().is_none(),
            "entry B (persistent, expired) data key should be dead in live bucket list"
        );
        assert!(
            bucket_list.get(&ttl_key_for(&key_b)).unwrap().is_none(),
            "TTL for entry B should be evicted from live bucket list"
        );
        assert!(
            bucket_list.get(&key_c).unwrap().is_none(),
            "entry C (temp, expired) should be evicted from live bucket list"
        );
        assert!(
            bucket_list.get(&ttl_key_for(&key_c)).unwrap().is_none(),
            "TTL for entry C should be evicted from live bucket list"
        );
        assert!(
            bucket_list.get(&key_d).unwrap().is_none(),
            "entry D (persistent, expired) data key should be dead in live bucket list"
        );
        assert!(
            bucket_list.get(&ttl_key_for(&key_d)).unwrap().is_none(),
            "TTL for entry D should be evicted from live bucket list"
        );

        // --- Assertion 2: Control entries E and F survive ---
        assert!(
            bucket_list.get(&key_e).unwrap().is_some(),
            "entry E (persistent, NOT expired) should still be in live bucket list"
        );
        assert!(
            bucket_list.get(&ttl_key_for(&key_e)).unwrap().is_some(),
            "TTL for entry E should still be in live bucket list"
        );
        assert!(
            bucket_list.get(&key_f).unwrap().is_some(),
            "entry F (temporary, NOT expired) should still be in live bucket list"
        );
        assert!(
            bucket_list.get(&ttl_key_for(&key_f)).unwrap().is_some(),
            "TTL for entry F should still be in live bucket list"
        );

        // --- Assertion 3: Persistent entries archived to hot archive ---
        assert!(
            hot_archive.get(&key_b).unwrap().is_some(),
            "entry B (persistent, expired) should be in hot archive"
        );
        assert!(
            hot_archive.get(&key_d).unwrap().is_some(),
            "entry D (persistent, expired) should be in hot archive"
        );

        // --- Assertion 4: Negative hot-archive assertions ---
        assert!(
            hot_archive.get(&key_a).unwrap().is_none(),
            "entry A (temporary) must NOT be in hot archive"
        );
        assert!(
            hot_archive.get(&key_c).unwrap().is_none(),
            "entry C (temporary) must NOT be in hot archive"
        );
        assert!(
            hot_archive.get(&key_e).unwrap().is_none(),
            "entry E (non-expired persistent) must NOT be in hot archive"
        );
        assert!(
            hot_archive.get(&key_f).unwrap().is_none(),
            "entry F (non-expired temporary) must NOT be in hot archive"
        );

        // --- Assertion 5: Eviction iterator updated ---
        assert!(
            result.eviction_iterator.is_some(),
            "eviction_iterator should be Some after eviction ran"
        );

        // --- Assertion 6: EvictionIterator persisted in bucket list ---
        let iter_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::EvictionIterator,
        });
        let persisted_iter = bucket_list
            .get(&iter_key)
            .unwrap()
            .expect("EvictionIterator config entry should be in bucket list");
        match persisted_iter.data {
            LedgerEntryData::ConfigSetting(ConfigSettingEntry::EvictionIterator(ref iter)) => {
                assert_eq!(
                    iter,
                    result.eviction_iterator.as_ref().unwrap(),
                    "persisted EvictionIterator should match result.eviction_iterator"
                );
            }
            _ => panic!("expected ConfigSettingEntry::EvictionIterator in bucket list"),
        }
    }
}
