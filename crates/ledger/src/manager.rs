//! Core ledger state management and coordination.
//!
//! This module provides [`LedgerManager`], the central component for managing
//! ledger state in rs-stellar-core. It coordinates between multiple subsystems
//! to ensure consistent state transitions during ledger close.
//!
//! # Responsibilities
//!
//! The [`LedgerManager`] is responsible for:
//!
//! - **State Management**: Maintaining the current ledger header
//! - **Bucket List Integration**: Updating the Merkle tree of ledger entries
//! - **Transaction Execution**: Coordinating transaction processing via [`close_ledger`](LedgerManager::close_ledger)
//! - **Snapshots**: Providing consistent point-in-time views for queries
//!
//! # Thread Safety
//!
//! The [`LedgerManager`] uses internal locking (`RwLock`) to allow concurrent
//! reads while serializing writes. Multiple threads can safely query the current
//! state while ledger close operations are serialized.
//!
//! # Hot Archive Support
//!
//! Starting with Protocol 23, the manager supports a hot archive bucket list
//! for state archival. This stores archived/evicted entries separately from
//! the live bucket list, and both contribute to the header's bucket list hash.

use crate::{
    close::{
        LedgerCloseData, LedgerCloseResult, LedgerCloseStats, TransactionSetVariant, UpgradeContext,
    },
    delta::{EntryChange, LedgerDelta},
    execution::{
        execute_soroban_parallel_phase, load_soroban_network_info,
        pre_deduct_all_fees_on_delta, run_transactions_on_executor,
        SorobanContext, SorobanNetworkInfo, TransactionExecutionResult,
        TransactionExecutor, TxSetResult,
    },
    header::{compute_header_hash, create_next_header},
    snapshot::{LedgerSnapshot, SnapshotHandle},
    LedgerError, Result,
};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use henyey_bucket::{
    BucketEntry, BucketEntryExt, BucketList, BucketListSnapshot, BucketMergeMap, EvictionIterator,
    EvictionIteratorExt, EvictionResult, HotArchiveBucketList,
};
use henyey_common::{BucketListDbConfig, Hash256, NetworkId};
use henyey_common::protocol::{
    needs_upgrade_to_version, protocol_version_starts_from, ProtocolVersion,
};
use henyey_tx::soroban::PersistentModuleCache;
use crate::offer_store::OfferStore;
use henyey_tx::{ClassicEventConfig, LedgerContext, TxEventManager};
use stellar_xdr::curr::{
    AccountId, BucketListType, ConfigSettingEntry, ConfigSettingId,
    ExtensionPoint, GeneralizedTransactionSet, Hash,
    LedgerCloseMeta, LedgerCloseMetaExt, LedgerCloseMetaExtV1, LedgerCloseMetaV2, LedgerEntry,
    LedgerEntryData, LedgerEntryExt, LedgerHeader, LedgerHeaderHistoryEntry,
    LedgerHeaderHistoryEntryExt, LedgerKey, LedgerKeyConfigSetting, PoolId,
    ScpHistoryEntry, StateArchivalSettings, TransactionEventStage, TransactionMeta,
    TransactionPhase, TransactionResultMetaV1, TransactionSet, TransactionSetV1,
    TxSetComponent, TxSetComponentTxsMaybeDiscountedFee, UpgradeEntryMeta, VecM,
};
use tracing::{debug, info};

/// Read current RSS (Resident Set Size) in bytes from /proc/self/statm.
/// Returns 0 on non-Linux or on error.
fn get_rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
            // Format: size resident shared text lib data dt (all in pages)
            let page_size = 4096u64; // standard Linux page size
            if let Some(rss_pages) = statm.split_whitespace().nth(1) {
                if let Ok(pages) = rss_pages.parse::<u64>() {
                    return pages * page_size;
                }
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Secondary index type: account → set of pool_ids for pool share trustlines.
type PoolShareTlAccountIndex = HashMap<AccountId, HashSet<PoolId>>;

/// Prepend a fee event to transaction metadata.
///
/// This adds a "NewFee" event at the beginning of the transaction's event list
/// to record the fee charged. Used for Protocol 20+ classic event emission.
///
/// # Arguments
///
/// * `meta` - The transaction metadata to modify
/// * `fee_source` - The account that paid the fee
/// * `fee_charged` - The amount of fee charged in stroops
/// * `protocol_version` - The current protocol version
/// * `network_id` - The network identifier
/// * `classic_events` - Classic event configuration
pub fn prepend_fee_event(
    meta: &mut TransactionMeta,
    fee_source: &AccountId,
    fee_charged: i64,
    protocol_version: u32,
    network_id: &NetworkId,
    classic_events: ClassicEventConfig,
) {
    if fee_charged == 0 || !classic_events.events_enabled(protocol_version) {
        return;
    }

    let mut manager = TxEventManager::new(true, protocol_version, *network_id, classic_events);
    manager.new_fee_event(fee_source, fee_charged, TransactionEventStage::BeforeAllTxs);
    let fee_events = manager.finalize();
    if fee_events.is_empty() {
        return;
    }

    if let TransactionMeta::V4(ref mut v4) = meta {
        let existing_events: Vec<stellar_xdr::curr::TransactionEvent> =
            v4.events.iter().cloned().collect();
        let mut combined = Vec::with_capacity(fee_events.len() + existing_events.len());
        combined.extend(fee_events);
        combined.extend(existing_events);
        v4.events = combined.try_into().unwrap_or_default();
    }
}

/// A background eviction scan that was started after committing a ledger.
///
/// After committing ledger N, a background thread scans for entries to evict
/// at ledger N+1. When N+1 arrives, the result is resolved (TTL filtering +
/// max_entries limit) instead of running the scan inline.
struct PendingEvictionScan {
    handle: std::thread::JoinHandle<henyey_bucket::Result<EvictionResult>>,
    target_ledger_seq: u32,
    settings: StateArchivalSettings,
}

/// Pre-computed cache data from a bucket list scan.
///
/// This struct captures all the data that `initialize_all_caches` would compute,
/// allowing the scan to run concurrently with other operations (e.g., merge restarts)
/// during catchup. The scan runs on level Arc pairs (read-only, `Send + Sync`)
/// so it can execute on a background thread concurrently with merge restarts
/// that only modify `level.next`.
pub struct CacheInitResult {
    /// All live offers indexed by offer_id.
    offers: HashMap<i64, LedgerEntry>,
    /// Secondary index: account → set of pool_ids for pool share trustlines.
    pool_share_tl_account_index: HashMap<AccountId, HashSet<PoolId>>,
    /// Pre-compiled Soroban module cache (if Soroban is supported).
    module_cache: Option<PersistentModuleCache>,
    /// In-memory Soroban state (contract data, code, TTLs, config).
    soroban_state: crate::soroban_state::InMemorySorobanState,
}

/// Result of scanning a single bucket level's curr+snap buckets.
///
/// Each level produces its own set of live entries and TTL entries, with
/// intra-level dedup (curr shadows snap). Cross-level dedup happens during merge.
struct LevelScanResult {
    /// Live entries of interest, deduped within this level (curr shadows snap).
    entries: HashMap<LedgerKey, LedgerEntry>,
    /// TTL entries keyed by TTL key hash (separate because TTLs need special handling).
    ttl_entries: HashMap<Hash, (stellar_xdr::curr::LedgerKeyTtl, crate::soroban_state::TtlData)>,
    /// Keys that were DEAD at this level — used for cross-level shadowing.
    /// A dead entry at a lower level must prevent live entries at higher levels
    /// from being included in the final result.
    dead_keys: HashSet<LedgerKey>,
    /// TTL key hashes that were DEAD at this level.
    dead_ttl_keys: HashSet<Hash>,
}

/// Accumulator for scanning a single bucket level.
///
/// Holds the mutable state built up during a scan of a level's curr+snap buckets.
/// The `process_entry` method is the core logic shared by both the fast path
/// (pre-collected entries) and the fallback path (full bucket iteration).
struct LevelScanner {
    entries: HashMap<LedgerKey, LedgerEntry>,
    ttl_entries: HashMap<Hash, (stellar_xdr::curr::LedgerKeyTtl, crate::soroban_state::TtlData)>,
    seen_keys: HashSet<LedgerKey>,
    dead_keys: HashSet<LedgerKey>,
    dead_ttl_keys: HashSet<Hash>,
}

impl LevelScanner {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            ttl_entries: HashMap::new(),
            seen_keys: HashSet::new(),
            dead_keys: HashSet::new(),
            dead_ttl_keys: HashSet::new(),
        }
    }

    fn into_result(self) -> LevelScanResult {
        LevelScanResult {
            entries: self.entries,
            ttl_entries: self.ttl_entries,
            dead_keys: self.dead_keys,
            dead_ttl_keys: self.dead_ttl_keys,
        }
    }

    /// Process a single scan-relevant entry, updating the level's result maps.
    fn process_entry(
        &mut self,
        entry: &BucketEntry,
        key: LedgerKey,
        soroban_enabled: bool,
        module_cache: &Option<Arc<PersistentModuleCache>>,
        protocol_version: u32,
    ) {
        if self.seen_keys.contains(&key) {
            return;
        }

        // Skip soroban types if not enabled
        if !soroban_enabled && !matches!(&key, LedgerKey::Offer(_)) {
            return;
        }

        self.seen_keys.insert(key.clone());

        if let BucketEntry::Liveentry(ref le) | BucketEntry::Initentry(ref le) = entry {
            // Compile contracts in parallel across levels via the shared module cache
            if let LedgerEntryData::ContractCode(ref contract_code) = le.data {
                if let Some(ref cache) = module_cache {
                    cache.add_contract(contract_code.code.as_slice(), protocol_version);
                }
            }

            // TTL entries go into a separate map keyed by hash
            if let LedgerEntryData::Ttl(ref ttl) = le.data {
                let ttl_key = stellar_xdr::curr::LedgerKeyTtl {
                    key_hash: ttl.key_hash.clone(),
                };
                let ttl_data = crate::soroban_state::TtlData::new(
                    ttl.live_until_ledger_seq,
                    le.last_modified_ledger_seq,
                );
                self.ttl_entries.insert(ttl.key_hash.clone(), (ttl_key, ttl_data));
            } else {
                self.entries.insert(key, le.clone());
            }
        } else if let BucketEntry::Deadentry(_) = entry {
            // Track dead keys so they shadow live entries at higher (older) levels.
            // For TTL entries, also track in the TTL-specific dead set.
            if let LedgerKey::Ttl(ref ttl_key) = key {
                self.dead_ttl_keys.insert(ttl_key.key_hash.clone());
            }
            self.dead_keys.insert(key);
        }
    }
}

fn scan_single_level(
    curr: &henyey_bucket::Bucket,
    snap: &henyey_bucket::Bucket,
    soroban_enabled: bool,
    module_cache: &Option<Arc<PersistentModuleCache>>,
    protocol_version: u32,
) -> LevelScanResult {
    let mut scanner = LevelScanner::new();

    // Scan curr first, then snap (curr shadows snap within a level)
    for bucket in [curr, snap] {
        for entry in bucket.iter() {
            let key = match entry.key() {
                Some(k) => k,
                None => continue, // metadata
            };

            // Filter to scan-relevant types only
            let is_scan_relevant = matches!(
                &key,
                LedgerKey::Offer(_)
                    | LedgerKey::ContractCode(_)
                    | LedgerKey::ContractData(_)
                    | LedgerKey::Ttl(_)
                    | LedgerKey::ConfigSetting(_)
            ) || matches!(&key, LedgerKey::Trustline(tl_key)
                if matches!(tl_key.asset, stellar_xdr::curr::TrustLineAsset::PoolShare(_)));
            if !is_scan_relevant {
                continue;
            }

            scanner.process_entry(
                &entry,
                key,
                soroban_enabled,
                module_cache,
                protocol_version,
            );
        }
    }

    scanner.into_result()
}

/// Merge per-level scan results into a single `CacheInitResult`.
///
/// Processes levels in order (0 → 10) so that lower-numbered levels (newer data)
/// shadow higher-numbered levels. A global seen set tracks cross-level dedup.
#[cfg(test)]
fn merge_level_results(
    level_results: Vec<LevelScanResult>,
    module_cache: Option<PersistentModuleCache>,
    protocol_version: u32,
    rent_config: &Option<crate::soroban_state::SorobanRentConfig>,
) -> CacheInitResult {
    let mut soroban_state = crate::soroban_state::InMemorySorobanState::new();
    let mut mem_offers: HashMap<i64, LedgerEntry> = HashMap::new();
    let mut pool_share_tl_account_index: HashMap<AccountId, HashSet<PoolId>> = HashMap::new();
    let mut global_seen: HashSet<LedgerKey> = HashSet::new();
    let mut global_ttl_seen: HashSet<Hash> = HashSet::new();

    let mut offer_count = 0u64;
    let mut code_count = 0u64;
    let mut data_count = 0u64;
    let mut ttl_count = 0u64;
    let mut config_count = 0u64;

    for level_result in level_results {
        // Register dead keys from this level into the global seen set.
        // Dead entries at lower (newer) levels must shadow live entries at
        // higher (older) levels, preventing stale data from being included.
        for dead_key in level_result.dead_keys {
            global_seen.insert(dead_key);
        }
        for dead_ttl_hash in level_result.dead_ttl_keys {
            global_ttl_seen.insert(dead_ttl_hash);
        }

        // Process non-TTL entries
        for (key, entry) in level_result.entries {
            if !global_seen.insert(key) {
                continue; // Already seen in a lower (newer) level
            }
            match &entry.data {
                LedgerEntryData::Offer(ref offer) => {
                    let offer_id = offer.offer_id;
                    mem_offers.insert(offer_id, entry);
                    offer_count += 1;
                }
                LedgerEntryData::Trustline(ref tl) => {
                    if let stellar_xdr::curr::TrustLineAsset::PoolShare(ref pool_id) = tl.asset {
                        pool_share_tl_account_index
                            .entry(tl.account_id.clone())
                            .or_default()
                            .insert(pool_id.clone());
                    }
                }
                LedgerEntryData::ContractCode(_) => {
                    // Module cache was already populated during scan_single_level
                    if let Err(e) = soroban_state.create_contract_code(
                        entry,
                        protocol_version,
                        rent_config.as_ref(),
                    ) {
                        tracing::warn!(error = %e, "Failed to add contract code to soroban state");
                    } else {
                        code_count += 1;
                    }
                }
                LedgerEntryData::ContractData(_) => {
                    if let Err(e) = soroban_state.create_contract_data(entry) {
                        tracing::warn!(error = %e, "Failed to add contract data to soroban state");
                    } else {
                        data_count += 1;
                    }
                }
                LedgerEntryData::ConfigSetting(_) => {
                    if let Err(e) = soroban_state.process_entry_create(
                        &entry,
                        protocol_version,
                        rent_config.as_ref(),
                    ) {
                        tracing::warn!(error = %e, "Failed to add config setting to soroban state");
                    } else {
                        config_count += 1;
                    }
                }
                _ => {}
            }
        }

        // Process TTL entries
        for (key_hash, (ttl_key, ttl_data)) in level_result.ttl_entries {
            if !global_ttl_seen.insert(key_hash) {
                continue;
            }
            if let Err(e) = soroban_state.create_ttl(&ttl_key, ttl_data) {
                tracing::trace!(error = %e, "Failed to add TTL to soroban state (may be pending)");
            } else {
                ttl_count += 1;
            }
        }
    }

    info!(
        offer_count,
        code_count,
        data_count,
        ttl_count,
        config_count,
        "scan_bucket_list_for_caches: merge complete"
    );

    CacheInitResult {
        offers: mem_offers,
        pool_share_tl_account_index,
        module_cache,
        soroban_state,
    }
}

/// Bounded-parallel scan-and-merge: up to `scan_thread_count` threads scan levels
/// concurrently while the main thread merges completed results in level order (0 → 10).
///
/// Levels are claimed in largest-first order so that the biggest (slowest) levels
/// start immediately on all available workers, minimizing wall-clock time.
///
/// Memory is bounded to N concurrent raw `LevelScanResult`s plus accumulated final state,
/// because the main thread merges and drops each raw HashMap before the window slides forward.
/// With N=1 this is equivalent to the previous sequential loop.
fn scan_and_merge(
    level_pairs: &[(Arc<henyey_bucket::Bucket>, Arc<henyey_bucket::Bucket>)],
    protocol_version: u32,
    soroban_enabled: bool,
    rent_config: &Option<crate::soroban_state::SorobanRentConfig>,
    module_cache: Option<PersistentModuleCache>,
    scan_thread_count: usize,
) -> CacheInitResult {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    let module_cache_arc = module_cache.map(Arc::new);
    let num_levels = level_pairs.len();
    let num_workers = scan_thread_count.min(num_levels).max(1);

    let mut soroban_state = crate::soroban_state::InMemorySorobanState::new();
    let mut mem_offers: HashMap<i64, LedgerEntry> = HashMap::new();
    let mut pool_share_tl_account_index: HashMap<AccountId, HashSet<PoolId>> = HashMap::new();
    let mut global_seen: HashSet<LedgerKey> = HashSet::new();
    let mut global_ttl_seen: HashSet<Hash> = HashSet::new();

    let mut offer_count = 0u64;
    let mut code_count = 0u64;
    let mut data_count = 0u64;
    let mut ttl_count = 0u64;
    let mut config_count = 0u64;

    // Sort level indices by total bucket size descending for better load balancing:
    // large levels (which take longest) start immediately, small levels fill gaps.
    let mut sorted_indices: Vec<usize> = (0..num_levels).collect();
    sorted_indices.sort_by_key(|&i| {
        std::cmp::Reverse(level_pairs[i].0.len() + level_pairs[i].1.len())
    });

    // Workers atomically claim slots in the sorted array.
    let next_claim = AtomicUsize::new(0);
    let next_claim_ref = &next_claim;
    let sorted_ref = &sorted_indices[..];
    let (tx, rx) = mpsc::channel::<(usize, LevelScanResult)>();

    std::thread::scope(|s| {
        for _ in 0..num_workers {
            let worker_tx = tx.clone();
            let mc_clone = module_cache_arc.clone();
            s.spawn(move || {
                loop {
                    let claim_idx = next_claim_ref.fetch_add(1, Ordering::Relaxed);
                    if claim_idx >= num_levels {
                        break;
                    }
                    let idx = sorted_ref[claim_idx];
                    let (curr, snap) = &level_pairs[idx];
                    let level_start = std::time::Instant::now();
                    let result = scan_single_level(
                        curr,
                        snap,
                        soroban_enabled,
                        &mc_clone,
                        protocol_version,
                    );
                    info!(
                        level = idx,
                        entries = result.entries.len(),
                        ttls = result.ttl_entries.len(),
                        elapsed_ms = level_start.elapsed().as_millis() as u64,
                        "scan_bucket_list_for_caches: level scan complete"
                    );
                    if worker_tx.send((idx, result)).is_err() {
                        break; // receiver dropped (shouldn't happen)
                    }
                }
            });
        }

        // Receive exactly `num_levels` results (one per level) and merge in order.
        // Buffer out-of-order completions; drain consecutive completed levels as
        // their turn arrives so raw HashMaps are freed as early as possible.
        let mut pending: HashMap<usize, LevelScanResult> = HashMap::new();
        let mut next_merge_idx = 0usize;

        for _ in 0..num_levels {
            let (idx, result) = rx.recv().expect("worker thread panicked");
            pending.insert(idx, result);
            while let Some(result) = pending.remove(&next_merge_idx) {
                // --- merge this level into accumulators, then drop raw result ---
                for dead_key in result.dead_keys {
                    global_seen.insert(dead_key);
                }
                for dead_ttl_hash in result.dead_ttl_keys {
                    global_ttl_seen.insert(dead_ttl_hash);
                }
                for (key, entry) in result.entries {
                    if !global_seen.insert(key) {
                        continue;
                    }
                    match &entry.data {
                        LedgerEntryData::Offer(ref offer) => {
                            let offer_id = offer.offer_id;
                            mem_offers.insert(offer_id, entry);
                            offer_count += 1;
                        }
                        LedgerEntryData::Trustline(ref tl) => {
                            if let stellar_xdr::curr::TrustLineAsset::PoolShare(ref pool_id) = tl.asset {
                                pool_share_tl_account_index
                                    .entry(tl.account_id.clone())
                                    .or_default()
                                    .insert(pool_id.clone());
                            }
                        }
                        LedgerEntryData::ContractCode(_) => {
                            if let Err(e) = soroban_state.create_contract_code(
                                entry,
                                protocol_version,
                                rent_config.as_ref(),
                            ) {
                                tracing::warn!(error = %e, "Failed to add contract code to soroban state");
                            } else {
                                code_count += 1;
                            }
                        }
                        LedgerEntryData::ContractData(_) => {
                            if let Err(e) = soroban_state.create_contract_data(entry) {
                                tracing::warn!(error = %e, "Failed to add contract data to soroban state");
                            } else {
                                data_count += 1;
                            }
                        }
                        LedgerEntryData::ConfigSetting(_) => {
                            if let Err(e) = soroban_state.process_entry_create(
                                &entry,
                                protocol_version,
                                rent_config.as_ref(),
                            ) {
                                tracing::warn!(error = %e, "Failed to add config setting to soroban state");
                            } else {
                                config_count += 1;
                            }
                        }
                        _ => {}
                    }
                }
                for (key_hash, (ttl_key, ttl_data)) in result.ttl_entries {
                    if !global_ttl_seen.insert(key_hash) {
                        continue;
                    }
                    if let Err(e) = soroban_state.create_ttl(&ttl_key, ttl_data) {
                        tracing::trace!(error = %e, "Failed to add TTL to soroban state (may be pending)");
                    } else {
                        ttl_count += 1;
                    }
                }
                // LevelScanResult dropped here — raw HashMaps freed before next level.
                next_merge_idx += 1;
            }
        }
    });

    info!(
        offer_count,
        code_count,
        data_count,
        ttl_count,
        config_count,
        "scan_bucket_list_for_caches: merge complete"
    );

    let module_cache = module_cache_arc.map(|arc| {
        Arc::try_unwrap(arc).unwrap_or_else(|arc| PersistentModuleCache::clone(&arc))
    });

    CacheInitResult {
        offers: mem_offers,
        pool_share_tl_account_index,
        module_cache,
        soroban_state,
    }
}

/// Scan a bucket list and extract all cache data.
///
/// Extracts level pairs from the BucketList, then delegates to
/// `scan_level_pairs_for_caches` which computes the rent config internally.
fn scan_bucket_list_for_caches(
    bucket_list: &BucketList,
    protocol_version: u32,
    scan_thread_count: usize,
) -> CacheInitResult {
    let level_pairs: Vec<(Arc<henyey_bucket::Bucket>, Arc<henyey_bucket::Bucket>)> = bucket_list
        .levels()
        .iter()
        .map(|l| (l.curr.clone(), l.snap.clone()))
        .collect();
    scan_level_pairs_for_caches(level_pairs, protocol_version, scan_thread_count)
}

/// Look up an entry from a slice of (curr, snap) bucket pairs, scanning level-order.
///
/// Equivalent to `BucketList::get` but operates on pre-extracted pairs so it can
/// be called without holding a `BucketList` reference.
fn get_from_pairs(
    level_pairs: &[(Arc<henyey_bucket::Bucket>, Arc<henyey_bucket::Bucket>)],
    key: &LedgerKey,
) -> henyey_bucket::Result<Option<LedgerEntry>> {
    use henyey_bucket::BucketEntry;
    'level: for (curr, snap) in level_pairs {
        for bucket in [curr, snap] {
            if let Some(entry) = bucket.get(key)? {
                match entry {
                    BucketEntry::Liveentry(e) | BucketEntry::Initentry(e) => return Ok(Some(e)),
                    BucketEntry::Deadentry(_) => return Ok(None),
                    BucketEntry::Metaentry(_) => continue 'level,
                }
            }
        }
    }
    Ok(None)
}

/// Build a `SorobanRentConfig` from a lookup function that retrieves ledger entries by key.
///
/// Both `compute_soroban_rent_config_from_pairs` and `LedgerManager::load_soroban_rent_config`
/// perform the same 3-key extraction; this helper centralizes that logic.
fn build_soroban_rent_config(
    mut lookup: impl FnMut(&LedgerKey) -> Option<LedgerEntry>,
) -> Option<crate::soroban_state::SorobanRentConfig> {
    let cpu_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: ConfigSettingId::ContractCostParamsCpuInstructions,
    });
    let cpu_params = lookup(&cpu_key).and_then(|e| {
        if let LedgerEntryData::ConfigSetting(
            ConfigSettingEntry::ContractCostParamsCpuInstructions(params),
        ) = e.data
        {
            Some(params)
        } else {
            None
        }
    })?;

    let mem_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: ConfigSettingId::ContractCostParamsMemoryBytes,
    });
    let mem_params = lookup(&mem_key).and_then(|e| {
        if let LedgerEntryData::ConfigSetting(
            ConfigSettingEntry::ContractCostParamsMemoryBytes(params),
        ) = e.data
        {
            Some(params)
        } else {
            None
        }
    })?;

    let compute_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: ConfigSettingId::ContractComputeV0,
    });
    let (tx_max_instructions, tx_max_memory_bytes) = lookup(&compute_key).and_then(|e| {
        if let LedgerEntryData::ConfigSetting(ConfigSettingEntry::ContractComputeV0(compute)) =
            e.data
        {
            Some((
                compute.tx_max_instructions as u64,
                compute.tx_memory_limit as u64,
            ))
        } else {
            None
        }
    })?;

    Some(crate::soroban_state::SorobanRentConfig {
        cpu_cost_params: cpu_params,
        mem_cost_params: mem_params,
        tx_max_instructions,
        tx_max_memory_bytes,
    })
}

/// Compute Soroban rent config from pre-extracted bucket level pairs via point lookups.
fn compute_soroban_rent_config_from_pairs(
    level_pairs: &[(Arc<henyey_bucket::Bucket>, Arc<henyey_bucket::Bucket>)],
) -> Option<crate::soroban_state::SorobanRentConfig> {
    build_soroban_rent_config(|key| get_from_pairs(level_pairs, key).ok().flatten())
}

/// Scan a set of bucket level pairs and extract all cache data.
///
/// This is the primary scan entry point. It accepts pre-extracted `Arc<Bucket>` pairs
/// (one per level) so it can run on a background thread concurrently with merge restarts,
/// which only modify `level.next` and never touch `level.curr` or `level.snap`.
///
/// Rent config is computed from the pairs via point lookups at the start of this function,
/// inside whatever thread context the caller provides (typically `spawn_blocking`).
///
/// Up to `scan_thread_count` levels are scanned concurrently in largest-first order;
/// results are merged in level order (level 0 wins) to preserve correct shadowing
/// semantics while bounding peak memory to N concurrent raw scan results.
pub fn scan_level_pairs_for_caches(
    level_pairs: Vec<(Arc<henyey_bucket::Bucket>, Arc<henyey_bucket::Bucket>)>,
    protocol_version: u32,
    scan_thread_count: usize,
) -> CacheInitResult {
    use henyey_common::MIN_SOROBAN_PROTOCOL_VERSION;

    let cache_init_start = std::time::Instant::now();

    let soroban_enabled = protocol_version >= MIN_SOROBAN_PROTOCOL_VERSION;
    let module_cache = if soroban_enabled {
        PersistentModuleCache::new_for_protocol(protocol_version)
    } else {
        None
    };

    // Compute rent config from the pairs (3 point lookups). This runs in the
    // caller's thread context — for the startup path it runs inside spawn_blocking,
    // keeping the tokio thread unblocked.
    let rent_config = if soroban_enabled {
        compute_soroban_rent_config_from_pairs(&level_pairs)
    } else {
        None
    };

    info!(
        soroban_enabled,
        scan_thread_count,
        "scan_bucket_list_for_caches: starting scan..."
    );

    let result = scan_and_merge(
        &level_pairs,
        protocol_version,
        soroban_enabled,
        &rent_config,
        module_cache,
        scan_thread_count,
    );

    let scan_elapsed = cache_init_start.elapsed();
    info!(
        elapsed_ms = scan_elapsed.as_millis() as u64,
        "scan_bucket_list_for_caches: complete"
    );

    result
}

/// Load the EvictionIterator from the bucket list's ConfigSettingEntry.
///
/// The EvictionIterator tracks where the incremental eviction scan is positioned.
/// Returns `None` if no EvictionIterator entry exists (pre-protocol 23).
fn load_eviction_iterator_from_bucket_list(bucket_list: &BucketList) -> Option<EvictionIterator> {
    let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
        config_setting_id: ConfigSettingId::EvictionIterator,
    });

    match bucket_list.get(&key) {
        Ok(Some(entry)) => {
            if let LedgerEntryData::ConfigSetting(ConfigSettingEntry::EvictionIterator(iter)) =
                entry.data
            {
                Some(iter)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Configuration options for the [`LedgerManager`].
///
/// This struct controls various aspects of ledger processing behavior,
/// including validation and event emission.
///
/// # Defaults
///
/// The default configuration enables all validation, which is appropriate
/// for production use. For testing, you may want to disable certain
/// validations for faster execution.
#[derive(Debug, Clone)]
pub struct LedgerManagerConfig {
    /// Whether to validate bucket list hashes against header values.
    ///
    /// When enabled, the computed bucket list hash is verified against the
    /// expected hash in the ledger header. Disable for replay-only scenarios
    /// where hash verification is not needed.
    pub validate_bucket_hash: bool,

    /// Whether to emit classic (non-Soroban) contract events.
    ///
    /// When enabled, SAC (Stellar Asset Contract) events are generated
    /// for classic operations like payments and trustline changes.
    pub emit_classic_events: bool,

    /// Whether to backfill Stellar Asset events for pre-protocol 23 ledgers.
    ///
    /// When enabled during catchup, classic events are generated for
    /// historical ledgers that predate native event support.
    pub backfill_stellar_asset_events: bool,

    /// BucketListDB configuration for indexing and caching.
    ///
    /// Controls per-bucket caching and index page sizes. Applied to the
    /// bucket list during initialization.
    pub bucket_list_db: BucketListDbConfig,

    /// When true, include `LedgerCloseMetaExtV1` (with `sorobanFeeWrite1KB`)
    /// in `LedgerCloseMeta.ext`. Maps to stellar-core `EMIT_LEDGER_CLOSE_META_EXT_V1`.
    pub emit_ledger_close_meta_ext_v1: bool,

    /// When true, include `SorobanTransactionMetaExtV1` (with fee breakdown)
    /// in Soroban transaction meta. Maps to stellar-core `EMIT_SOROBAN_TRANSACTION_META_EXT_V1`.
    pub emit_soroban_tx_meta_ext_v1: bool,

    /// When true, include diagnostic events in `TransactionMetaV4.diagnostic_events`.
    /// Maps to stellar-core `ENABLE_SOROBAN_DIAGNOSTIC_EVENTS`.
    ///
    /// Note: The Soroban host always captures diagnostic events (`enable_diagnostics: true`).
    /// This flag controls whether they are included in the metadata stream output.
    pub enable_soroban_diagnostic_events: bool,

    /// Number of parallel threads for the startup bucket list cache scan.
    pub scan_thread_count: usize,
}

impl Default for LedgerManagerConfig {
    fn default() -> Self {
        Self {
            validate_bucket_hash: true,
            emit_classic_events: false,
            backfill_stellar_asset_events: false,
            bucket_list_db: BucketListDbConfig::default(),
            emit_ledger_close_meta_ext_v1: false,
            emit_soroban_tx_meta_ext_v1: false,
            enable_soroban_diagnostic_events: false,
            scan_thread_count: 4,
        }
    }
}

/// Internal state of the ledger manager.
///
/// This struct holds the mutable state that changes with each ledger close.
/// It is protected by an RwLock for thread-safe access.
struct LedgerState {
    /// Current ledger header (the most recently closed ledger).
    header: LedgerHeader,

    /// SHA-256 hash of the current header's XDR encoding.
    header_hash: Hash256,

    /// Whether the ledger manager has been initialized.
    ///
    /// The manager must be initialized (via `initialize` or
    /// by loading from database) before ledger close operations can begin.
    initialized: bool,
}

/// The core ledger manager for rs-stellar-core.
///
/// `LedgerManager` is the central coordinator for all ledger state operations.
/// It manages the lifecycle of ledger closes, from receiving externalized
/// transaction sets to committing the new ledger state.
///
/// # Architecture
///
/// The manager coordinates between several subsystems:
///
/// - **Bucket List**: The Merkle tree of all ledger entries, providing
///   cryptographic integrity for the state
/// - **Snapshots**: Point-in-time views for concurrent access
///
/// # Initialization
///
/// Before use, the manager must be initialized via one of:
///
/// - [`initialize`](Self::initialize): For catchup from history archives
/// - [`reset`](Self::reset): To clear state before re-initialization
///
/// # Ledger Close Flow
///
/// Call [`close_ledger`](Self::close_ledger) with the externalized data to
/// execute transactions and finalize the ledger in a single call.
///
/// # Thread Safety
///
/// All public methods are safe to call from multiple threads. Internal state
/// is protected by RwLocks to allow concurrent reads during ledger processing.
pub struct LedgerManager {
    /// Live bucket list containing all current ledger entries.
    ///
    /// Wrapped in Arc for efficient sharing with snapshots.
    bucket_list: Arc<RwLock<BucketList>>,

    /// Hot archive bucket list for Protocol 23+ state archival.
    ///
    /// Contains archived/evicted entries. When present, its hash is combined
    /// with the live bucket list hash for the header's bucket_list_hash.
    hot_archive_bucket_list: Arc<RwLock<Option<HotArchiveBucketList>>>,

    /// Network ID derived from SHA-256 of the passphrase.
    network_id: NetworkId,

    /// Current mutable ledger state.
    state: RwLock<LedgerState>,

    /// Configuration options.
    config: LedgerManagerConfig,

    /// Persistent module cache for Soroban WASM compilation.
    ///
    /// This cache stores pre-compiled WASM modules for contract code entries,
    /// significantly improving performance for Soroban transactions by avoiding
    /// repeated compilation of the same contract code.
    module_cache: RwLock<Option<PersistentModuleCache>>,

    /// Flag indicating whether the in-memory offer store has been populated.
    ///
    /// The offer store is populated once during initialization from the bucket list
    /// and updated as offers are created/modified/deleted during ledger closes.
    /// This avoids expensive full bucket list scans during orderbook operations.
    offers_initialized: Arc<RwLock<bool>>,

    /// Unified in-memory offer store: canonical data + all indexes + metadata.
    /// Populated during initialize_all_caches() and updated on each ledger close.
    /// Replaces both the old `offer_store` (HashMap<i64, LedgerEntry>) and
    /// `offer_account_asset_index` (secondary index).
    /// Wrapped in Arc for sharing with snapshot closures.
    offer_store: Arc<Mutex<OfferStore>>,

    /// Secondary index: account_bytes → set of pool_id_bytes for pool share trustlines.
    ///
    /// Built at initialization by scanning the bucket list for pool share trustlines.
    /// Maintained incrementally in `commit_close`.
    /// Used by `load_pool_share_trustlines_for_account_and_asset` to find pool share
    /// trustlines without a full bucket list scan (mirroring stellar-core's SQL
    /// `SELECT * FROM trustlines WHERE account_id=? AND asset_type=POOL_SHARE`).
    pool_share_tl_account_index: Arc<RwLock<PoolShareTlAccountIndex>>,

    /// In-memory Soroban state for Protocol 20+ contract data/code tracking.
    ///
    /// This tracks all CONTRACT_DATA, CONTRACT_CODE, and TTL entries in memory,
    /// maintaining cumulative size totals that are updated incrementally during
    /// ledger close. This avoids expensive full bucket list scans for state
    /// size computation (used for LiveSorobanStateSizeWindow).
    soroban_state: Arc<crate::soroban_state::SharedSorobanState>,

    /// Persistent transaction executor reused across ledger closes.
    ///
    /// The executor's offer cache (~911K entries on mainnet) is expensive to
    /// rebuild from scratch (~2.7s per ledger). By persisting the executor
    /// across ledger closes and calling `advance_to_ledger_preserving_offers`,
    /// offers are maintained incrementally while non-offer entries are cleared
    /// and reloaded from the authoritative bucket list.
    ///
    /// Set to `None` before initialization and after `reset()`. Populated on
    /// the first `close_ledger` call and reused on subsequent calls.
    executor: Mutex<Option<TransactionExecutor>>,

    /// Background eviction scan started after the previous ledger's commit.
    ///
    /// After committing ledger N, a background thread scans for entries to evict
    /// at ledger N+1 using a snapshot of the bucket list. When N+1 arrives, the
    /// result is resolved instead of running an inline scan, reducing close latency.
    pending_eviction_scan: Mutex<Option<PendingEvictionScan>>,

    /// Shared merge map for bucket merge deduplication.
    ///
    /// When set, completed merges are recorded here after each `add_batch`,
    /// and `prepare_with_normalization` checks it before starting new merges.
    /// This avoids redundant merge computations across restarts and catchup.
    finished_merges: Option<Arc<std::sync::RwLock<BucketMergeMap>>>,
}

// Compile-time assertion: LedgerManager must be Send + Sync for spawn_blocking.
#[allow(dead_code)]
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    fn _check() {
        assert_send_sync::<LedgerManager>();
    }
};

impl LedgerManager {
    /// Create a new ledger manager.
    ///
    /// The ledger starts uninitialized and must be initialized via
    /// `initialize` before ledger close operations can begin.
    pub fn new(network_passphrase: String, config: LedgerManagerConfig) -> Self {
        let network_id = NetworkId::from_passphrase(&network_passphrase);

        Self {
            bucket_list: Arc::new(RwLock::new(BucketList::default())),
            hot_archive_bucket_list: Arc::new(RwLock::new(None)),
            network_id,
            state: RwLock::new(LedgerState {
                header: create_genesis_header(),
                header_hash: Hash256::ZERO,
                initialized: false,
            }),
            config,
            module_cache: RwLock::new(None),
            offers_initialized: Arc::new(RwLock::new(false)),
            offer_store: Arc::new(Mutex::new(OfferStore::new())),
            pool_share_tl_account_index: Arc::new(RwLock::new(HashMap::new())),
            soroban_state: Arc::new(crate::soroban_state::SharedSorobanState::new()),
            executor: Mutex::new(None),
            pending_eviction_scan: Mutex::new(None),
            finished_merges: None,
        }
    }

    /// Get the network ID.
    pub fn network_id(&self) -> &NetworkId {
        &self.network_id
    }

    /// Check if the ledger has been initialized.
    pub fn is_initialized(&self) -> bool {
        self.state.read().initialized
    }

    /// Get the current ledger sequence number.
    pub fn current_ledger_seq(&self) -> u32 {
        self.state.read().header.ledger_seq
    }

    /// Get the current ledger header.
    pub fn current_header(&self) -> LedgerHeader {
        self.state.read().header.clone()
    }

    /// Get the current header hash.
    pub fn current_header_hash(&self) -> Hash256 {
        self.state.read().header_hash
    }

    /// Override the stored ledger header version for testing.
    ///
    /// This simulates a network upgrade beyond supported versions.
    #[doc(hidden)]
    pub fn set_header_version_for_test(&self, version: u32) {
        let mut state = self.state.write();
        state.header.ledger_version = version;
    }

    /// Override the stored ledger header and header hash for testing.
    ///
    /// Used by the `ApplyLoad` benchmark harness after directly populating
    /// the bucket list to advance the ledger state without going through
    /// a full ledger close.
    #[doc(hidden)]
    pub fn set_header_for_test(&self, header: LedgerHeader, header_hash: Hash256) {
        let mut state = self.state.write();
        state.header = header;
        state.header_hash = header_hash;
    }

    /// Get bucket list level hashes (curr, snap) for persistence.
    pub fn bucket_list_levels(&self) -> Vec<(Hash256, Hash256)> {
        let bucket_list = self.bucket_list.read();
        bucket_list
            .levels()
            .iter()
            .map(|level| (level.curr.hash(), level.snap.hash()))
            .collect()
    }

    /// Get a read guard to the live bucket list.
    ///
    /// This is used during HAS serialization to capture the full bucket list
    /// state including pending merges.
    pub fn bucket_list(&self) -> parking_lot::RwLockReadGuard<'_, BucketList> {
        self.bucket_list.read()
    }

    /// Get a write guard to the live bucket list.
    ///
    /// Used by the `ApplyLoad` benchmark harness to directly populate the
    /// bucket list with synthetic entries without closing ledgers.
    pub fn bucket_list_mut(&self) -> parking_lot::RwLockWriteGuard<'_, BucketList> {
        self.bucket_list.write()
    }

    /// Set the shared merge map for bucket merge deduplication.
    ///
    /// This wires the merge map from `BucketManager` into the `LedgerManager`,
    /// enabling two behaviors:
    /// 1. `prepare_with_normalization` checks the map before starting new merges
    /// 2. Completed merges are recorded in the map after each `add_batch`
    ///
    /// Should be called once during initialization, before any `close_ledger` calls.
    pub fn set_merge_map(&mut self, merge_map: Arc<std::sync::RwLock<BucketMergeMap>>) {
        self.bucket_list.write().set_merge_map(merge_map.clone());
        self.finished_merges = Some(merge_map);
    }

    /// Resolve all pending async merges in the bucket list.
    ///
    /// This must be called before cloning the bucket list, because
    /// `BucketLevel::clone()` drops unresolved async merges. After calling
    /// this, all pending merges are `PendingMerge::InMemory` and safe to clone.
    pub fn resolve_pending_bucket_merges(&self) {
        self.bucket_list.write().resolve_all_pending_merges();
    }

    /// Get a lock on the unified offer store for direct access.
    pub fn offer_store_lock(&self) -> parking_lot::MutexGuard<'_, OfferStore> {
        self.offer_store.lock()
    }

    /// Get a read lock on the hot archive bucket list.
    ///
    /// This is used during HAS serialization to capture the hot archive state
    /// for restart recovery (protocol >= 23).
    pub fn hot_archive_bucket_list(
        &self,
    ) -> parking_lot::RwLockReadGuard<'_, Option<HotArchiveBucketList>> {
        self.hot_archive_bucket_list.read()
    }

    /// Get a write guard to the hot archive bucket list.
    ///
    /// Used by the `ApplyLoad` benchmark harness to directly populate the
    /// hot archive bucket list with synthetic entries.
    pub fn hot_archive_bucket_list_mut(
        &self,
    ) -> parking_lot::RwLockWriteGuard<'_, Option<HotArchiveBucketList>> {
        self.hot_archive_bucket_list.write()
    }

    /// Get all bucket hashes referenced by the live and hot archive bucket lists.
    ///
    /// Used for garbage collection to determine which bucket files on disk
    /// are still needed.
    pub fn all_referenced_bucket_hashes(&self) -> Vec<Hash256> {
        let mut hashes = self.bucket_list.read().all_referenced_hashes();
        if let Some(ref hot_archive) = *self.hot_archive_bucket_list.read() {
            hashes.extend(hot_archive.all_referenced_hashes());
        }
        hashes
    }

    /// Initialize the ledger from bucket list state.
    ///
    /// This is used during catchup from history archives.
    ///
    /// # Arguments
    ///
    /// * `bucket_list` - The live bucket list
    /// * `hot_archive_bucket_list` - The hot archive bucket list
    /// * `header` - The ledger header to initialize with
    /// * `header_hash` - The authoritative hash of the header from the history archive
    pub fn initialize(
        &self,
        bucket_list: BucketList,
        hot_archive_bucket_list: HotArchiveBucketList,
        header: LedgerHeader,
        header_hash: Hash256,
    ) -> Result<()> {
        let protocol_version = header.ledger_version;
        self.verify_and_install_bucket_lists(bucket_list, hot_archive_bucket_list, header, header_hash)?;
        self.initialize_all_caches(protocol_version, 0)?;

        info!(
            ledger_seq = self.state.read().header.ledger_seq,
            header_hash = %self.state.read().header_hash.to_hex(),
            "Ledger initialized from buckets"
        );

        Ok(())
    }

    /// Initialize the ledger from bucket list state using a pre-computed cache scan.
    ///
    /// This is the optimized startup path. The caller pre-computes `cache_data` on a
    /// background thread while merge restarts run concurrently, then calls this method
    /// to install everything atomically.
    ///
    /// # Arguments
    ///
    /// * `bucket_list` - The live bucket list (merges already restarted)
    /// * `hot_archive_bucket_list` - The hot archive bucket list
    /// * `header` - The ledger header to initialize with
    /// * `header_hash` - The authoritative hash of the header from the history archive
    /// * `cache_data` - Pre-computed cache data from `scan_level_pairs_for_caches`
    pub fn initialize_with_precomputed_caches(
        &self,
        bucket_list: BucketList,
        hot_archive_bucket_list: HotArchiveBucketList,
        header: LedgerHeader,
        header_hash: Hash256,
        cache_data: CacheInitResult,
    ) -> Result<()> {
        self.verify_and_install_bucket_lists(bucket_list, hot_archive_bucket_list, header, header_hash)?;

        // Initialize per-bucket caches for all DiskIndex buckets.
        {
            let bucket_list = self.bucket_list.read();
            bucket_list.maybe_initialize_caches();
        }

        // Install pre-computed cache data.
        *self.offer_store.lock() = OfferStore::from_bucket_list_entries(cache_data.offers);
        *self.pool_share_tl_account_index.write() = cache_data.pool_share_tl_account_index;
        *self.module_cache.write() = cache_data.module_cache;
        *self.soroban_state.write() = cache_data.soroban_state;
        *self.offers_initialized.write() = true;

        info!(
            ledger_seq = self.state.read().header.ledger_seq,
            header_hash = %self.state.read().header_hash.to_hex(),
            "Ledger initialized from buckets with precomputed caches"
        );

        Ok(())
    }

    /// Verify bucket list hash against the header and install bucket lists + state.
    fn verify_and_install_bucket_lists(
        &self,
        bucket_list: BucketList,
        hot_archive_bucket_list: HotArchiveBucketList,
        header: LedgerHeader,
        header_hash: Hash256,
    ) -> Result<()> {
        let mut state = self.state.write();
        if state.initialized {
            return Err(LedgerError::AlreadyInitialized);
        }

        // Compute bucket list hash for verification.
        // For protocol >= 23, the hash is SHA256(live_hash || hot_archive_hash).
        // For earlier protocols, the hash is just the live bucket list hash.
        let live_hash = bucket_list.hash();
        let computed_hash =
            if protocol_version_starts_from(header.ledger_version, ProtocolVersion::V23) {
                use sha2::{Digest, Sha256};
                let hot_hash = hot_archive_bucket_list.hash();
                let mut hasher = Sha256::new();
                hasher.update(live_hash.as_bytes());
                hasher.update(hot_hash.as_bytes());
                let result = hasher.finalize();
                let mut bytes = [0u8; 32];
                bytes.copy_from_slice(&result);
                Hash256::from_bytes(bytes)
            } else {
                live_hash
            };

        let expected_hash = Hash256::from(header.bucket_list_hash.0);

        tracing::debug!(
            header_ledger_seq = header.ledger_seq,
            protocol_version = header.ledger_version,
            expected = %expected_hash.to_hex(),
            computed = %computed_hash.to_hex(),
            live_hash = %live_hash.to_hex(),
            hot_archive_hash = %hot_archive_bucket_list.hash().to_hex(),
            "Verifying bucket list hash"
        );

        if self.config.validate_bucket_hash && computed_hash != expected_hash {
            return Err(LedgerError::HashMismatch {
                expected: expected_hash.to_hex(),
                actual: computed_hash.to_hex(),
            });
        }

        // Install bucket lists
        *self.bucket_list.write() = bucket_list;
        *self.hot_archive_bucket_list.write() = Some(hot_archive_bucket_list);

        // Set the ledger sequence and BucketListDB config on bucket lists.
        {
            let mut bl = self.bucket_list.write();
            bl.set_ledger_seq(header.ledger_seq);
            bl.set_bucket_list_db_config(self.config.bucket_list_db.clone());
            // Re-wire merge map on the new bucket list if one is configured.
            if let Some(ref merge_map) = self.finished_merges {
                bl.set_merge_map(merge_map.clone());
            }
        }
        if let Some(ref mut habl) = *self.hot_archive_bucket_list.write() {
            habl.set_ledger_seq(header.ledger_seq);
        }

        state.header = header;
        state.header_hash = header_hash;
        state.initialized = true;

        Ok(())
    }

    /// Reset the ledger manager state for re-initialization.
    ///
    /// This clears all caches, bucket lists, and state to allow a fresh
    /// `initialize` call. Used when catchup needs to reset
    /// state while the ledger manager was already initialized (e.g., after
    /// falling behind in live mode).
    pub fn reset(&self) {
        debug!("Resetting ledger manager for catchup");

        // Clear bucket lists
        *self.bucket_list.write() = BucketList::default();
        *self.hot_archive_bucket_list.write() = None;

        // Explicitly drop old module cache to release memory
        let _ = self.module_cache.write().take();

        *self.offers_initialized.write() = false;
        self.soroban_state.write().clear();

        // Clear the persistent executor so offers are reloaded after re-initialization
        *self.executor.lock() = None;

        // Discard any pending background eviction scan
        *self.pending_eviction_scan.lock() = None;

        // Reset state
        let mut state = self.state.write();
        state.header = create_genesis_header();
        state.header_hash = Hash256::ZERO;
        state.initialized = false;

        debug!("Ledger manager reset complete");
    }

    /// Load Soroban rent config from bucket list for code size calculation.
    fn load_soroban_rent_config(
        &self,
        bucket_list: &BucketList,
    ) -> Option<crate::soroban_state::SorobanRentConfig> {
        build_soroban_rent_config(|key| bucket_list.get(key).ok().flatten())
    }

    /// Initialize all caches from the bucket list using a single-pass scan.
    ///
    /// Performs ONE pass over the entire bucket list, dispatching each entry
    /// to the appropriate handler based on its type. This reads ~24 GB of
    /// bucket data once, instead of 5x (~120 GB) with per-type scanning.
    ///
    /// A single `HashSet<LedgerKey>` is used for deduplication across all
    /// entry types. Since `LedgerKey` is a discriminated union, keys of
    /// different types never collide. Peak memory for the dedup set is
    /// ~5.3M keys (~700 MB) on mainnet.
    ///
    /// Entry types processed:
    /// - Offer -> in-memory offer store + secondary index
    /// - ContractCode -> module cache + soroban state
    /// - ContractData -> soroban state
    /// - TTL -> soroban state
    /// - ConfigSetting -> soroban state
    fn initialize_all_caches(&self, protocol_version: u32, _ledger_seq: u32) -> Result<()> {
        let rss_before = get_rss_bytes();

        let bucket_list = self.bucket_list.read();
        let cache_data = scan_bucket_list_for_caches(&bucket_list, protocol_version, self.config.scan_thread_count);
        let rss_after_scan = get_rss_bytes();

        // Initialize per-bucket caches for all DiskIndex buckets.
        // Uses proportional sizing based on the BucketListDB config.
        bucket_list.maybe_initialize_caches();
        let rss_after_bucket_cache = get_rss_bytes();
        drop(bucket_list);

        *self.offer_store.lock() = OfferStore::from_bucket_list_entries(cache_data.offers);
        *self.pool_share_tl_account_index.write() = cache_data.pool_share_tl_account_index;
        *self.module_cache.write() = cache_data.module_cache;
        *self.soroban_state.write() = cache_data.soroban_state;
        *self.offers_initialized.write() = true;
        let rss_after_install = get_rss_bytes();

        info!(
            before_mb = rss_before / (1024 * 1024),
            after_scan_mb = rss_after_scan / (1024 * 1024),
            after_bucket_cache_mb = rss_after_bucket_cache / (1024 * 1024),
            after_install_mb = rss_after_install / (1024 * 1024),
            "initialize_all_caches memory"
        );

        Ok(())
    }

    /// Close a ledger by executing transactions and committing state changes.
    ///
    /// This is the main entry point for ledger close in live mode. It:
    /// 1. Validates the close data against current state
    /// 2. Executes all transactions in the set
    /// 3. Updates bucket list, soroban state, and other caches
    /// 4. Computes and returns the new ledger header
    ///
    /// # Example
    ///
    /// ```ignore
    /// let close_data = LedgerCloseData::new(seq, tx_set, close_time, prev_hash);
    /// let result = manager.close_ledger(close_data)?;
    /// println!("Closed ledger {}", result.ledger_seq());
    /// ```
    pub fn close_ledger(
        &self,
        close_data: LedgerCloseData,
        runtime_handle: Option<tokio::runtime::Handle>,
    ) -> Result<LedgerCloseResult> {
        let rss_before = get_rss_bytes();
        let begin_start = std::time::Instant::now();
        let mut ctx = self.begin_close(close_data)?;
        ctx.runtime_handle = runtime_handle;
        ctx.timing_begin_close_us = begin_start.elapsed().as_micros() as u64;

        let tx_start = std::time::Instant::now();
        ctx.apply_transactions()?;
        ctx.timing_tx_exec_us = tx_start.elapsed().as_micros() as u64;

        ctx.commit(rss_before)
    }

    /// Begin closing a new ledger (internal).
    ///
    /// Returns a LedgerCloseContext for applying transactions and
    /// committing the ledger. This is called by `close_ledger`.
    fn begin_close(&self, close_data: LedgerCloseData) -> Result<LedgerCloseContext<'_>> {
        let state = self.state.read();
        if !state.initialized {
            return Err(LedgerError::NotInitialized);
        }

        // Fatal: protocol version is unsupported.
        // Version 0 is the genesis state before any protocol upgrade — it is
        // always allowed because the upcoming ledger close will apply the upgrade.
        let version = state.header.ledger_version;
        let min = henyey_common::protocol::MIN_LEDGER_PROTOCOL_VERSION;
        let max = henyey_common::protocol::CURRENT_LEDGER_PROTOCOL_VERSION;
        if version != 0 && (version < min || version > max) {
            tracing::error!(
                version,
                min_supported = min,
                max_supported = max,
                "FATAL: ledger protocol version is outside supported range. \
                 The node cannot process this ledger."
            );
            panic!(
                "unsupported protocol version: {} (supported range: {}..={})",
                version, min, max,
            );
        }

        // Validate sequence
        let expected_seq = state.header.ledger_seq + 1;
        if close_data.ledger_seq != expected_seq {
            return Err(LedgerError::InvalidSequence {
                expected: expected_seq,
                actual: close_data.ledger_seq,
            });
        }

        // Validate previous hash
        if close_data.prev_ledger_hash != state.header_hash {
            // Describe the StellarValueExt for logging with details
            let stellar_value_ext_desc = match &state.header.scp_value.ext {
                stellar_xdr::curr::StellarValueExt::Basic => "Basic".to_string(),
                stellar_xdr::curr::StellarValueExt::Signed(sig) => {
                    let node_id_bytes = match &sig.node_id.0 {
                        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key) => key.0,
                    };
                    format!(
                        "Signed(node_id={}, sig_len={})",
                        Hash256::from_bytes(node_id_bytes).to_hex(),
                        sig.signature.len()
                    )
                }
            };

            // Compute recomputed hash to verify
            use stellar_xdr::curr::{Limits, WriteXdr};
            let header_xdr = state.header.to_xdr(Limits::none()).unwrap_or_default();
            let header_xdr_hex = Hash256::from_bytes({
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&header_xdr[..std::cmp::min(32, header_xdr.len())]);
                arr
            })
            .to_hex();
            tracing::error!(
                header_xdr_first_32_bytes = %header_xdr_hex,
                header_xdr_len = header_xdr.len(),
                "Header XDR bytes for debugging"
            );

            // Debug: Log header details to help diagnose hash mismatch
            let skip_list_0 = Hash256::from_bytes(state.header.skip_list[0].0).to_hex();
            let skip_list_1 = Hash256::from_bytes(state.header.skip_list[1].0).to_hex();
            let skip_list_2 = Hash256::from_bytes(state.header.skip_list[2].0).to_hex();
            let skip_list_3 = Hash256::from_bytes(state.header.skip_list[3].0).to_hex();
            tracing::error!(
                current_seq = state.header.ledger_seq,
                close_seq = close_data.ledger_seq,
                our_hash = %state.header_hash.to_hex(),
                network_prev_hash = %close_data.prev_ledger_hash.to_hex(),
                header_version = state.header.ledger_version,
                header_bucket_list_hash = %Hash256::from(state.header.bucket_list_hash.0).to_hex(),
                header_tx_result_hash = %Hash256::from(state.header.tx_set_result_hash.0).to_hex(),
                header_total_coins = state.header.total_coins,
                header_fee_pool = state.header.fee_pool,
                header_close_time = state.header.scp_value.close_time.0,
                header_tx_set_hash = %Hash256::from(state.header.scp_value.tx_set_hash.0).to_hex(),
                header_upgrades_count = state.header.scp_value.upgrades.len(),
                header_stellar_value_ext = %stellar_value_ext_desc,
                header_prev_ledger_hash = %Hash256::from(state.header.previous_ledger_hash.0).to_hex(),
                header_id_pool = state.header.id_pool,
                header_inflation_seq = state.header.inflation_seq,
                header_base_fee = state.header.base_fee,
                header_base_reserve = state.header.base_reserve,
                header_max_tx_set_size = state.header.max_tx_set_size,
                skip_list_0 = %skip_list_0,
                skip_list_1 = %skip_list_1,
                skip_list_2 = %skip_list_2,
                skip_list_3 = %skip_list_3,
                "Hash mismatch - our computed header hash differs from network's prev_ledger_hash"
            );

            // Log detailed bucket list state for debugging hash mismatch
            // This helps identify which specific level has diverged
            {
                let bucket_list = self.bucket_list.read();
                let live_hash = bucket_list.hash();
                tracing::error!(
                    ledger_seq = state.header.ledger_seq,
                    bucket_list_ledger_seq = bucket_list.ledger_seq(),
                    live_bucket_list_hash = %live_hash.to_hex(),
                    "HASH_MISMATCH_DEBUG: Live bucket list state"
                );

                // Log each level's curr and snap hashes
                for (level, level_hash, curr_hash, snap_hash) in bucket_list.level_hashes() {
                    tracing::error!(
                        ledger_seq = state.header.ledger_seq,
                        level = level,
                        level_hash = %level_hash.to_hex(),
                        curr_hash = %curr_hash.to_hex(),
                        snap_hash = %snap_hash.to_hex(),
                        "HASH_MISMATCH_DEBUG: Live bucket list level"
                    );
                }

                // Log hot archive state if present
                let hot_archive = self.hot_archive_bucket_list.read();
                if let Some(ref ha) = *hot_archive {
                    let hot_hash = ha.hash();
                    tracing::error!(
                        ledger_seq = state.header.ledger_seq,
                        hot_archive_ledger_seq = ha.ledger_seq(),
                        hot_archive_hash = %hot_hash.to_hex(),
                        "HASH_MISMATCH_DEBUG: Hot archive bucket list state"
                    );

                    for (level, level_hash, curr_hash, snap_hash) in ha.level_hashes() {
                        tracing::error!(
                            ledger_seq = state.header.ledger_seq,
                            level = level,
                            level_hash = %level_hash.to_hex(),
                            curr_hash = %curr_hash.to_hex(),
                            snap_hash = %snap_hash.to_hex(),
                            "HASH_MISMATCH_DEBUG: Hot archive bucket list level"
                        );
                    }

                    // Log the combined hash computation
                    use sha2::{Digest, Sha256};
                    let mut hasher = Sha256::new();
                    hasher.update(live_hash.as_bytes());
                    hasher.update(hot_hash.as_bytes());
                    let result = hasher.finalize();
                    let mut bytes = [0u8; 32];
                    bytes.copy_from_slice(&result);
                    let combined_hash = Hash256::from_bytes(bytes);
                    tracing::error!(
                        ledger_seq = state.header.ledger_seq,
                        live_hash = %live_hash.to_hex(),
                        hot_hash = %hot_hash.to_hex(),
                        combined_hash = %combined_hash.to_hex(),
                        header_bucket_list_hash = %Hash256::from(state.header.bucket_list_hash.0).to_hex(),
                        "HASH_MISMATCH_DEBUG: Combined bucket list hash computation"
                    );
                } else {
                    tracing::error!(
                        ledger_seq = state.header.ledger_seq,
                        "HASH_MISMATCH_DEBUG: No hot archive bucket list present!"
                    );
                }
            }

            return Err(LedgerError::HashMismatch {
                expected: state.header_hash.to_hex(),
                actual: close_data.prev_ledger_hash.to_hex(),
            });
        }

        // Create snapshot of current state for reading during close
        let snapshot = self.create_snapshot()?;

        let mut upgrade_ctx = UpgradeContext::new(state.header.ledger_version);
        for upgrade in &close_data.upgrades {
            upgrade_ctx.add_upgrade(upgrade.clone());
        }

        Ok(LedgerCloseContext {
            manager: self,
            close_data,
            prev_header: state.header.clone(),
            prev_header_hash: state.header_hash,
            delta: LedgerDelta::new(expected_seq),
            snapshot,
            stats: LedgerCloseStats::new(),
            upgrade_ctx,
            id_pool: state.header.id_pool,
            tx_results: Vec::new(),
            tx_result_metas: Vec::new(),
            hot_archive_restored_keys: Vec::new(),
            runtime_handle: None,
            start: std::time::Instant::now(),
            timing_begin_close_us: 0,
            timing_tx_exec_us: 0,
            timing_classic_exec_us: 0,
            timing_soroban_exec_us: 0,
            timing_prepare_us: 0,
            timing_config_load_us: 0,
            timing_executor_setup_us: 0,
            timing_fee_pre_deduct_us: 0,
            timing_post_exec_us: 0,
            tx_perf: Vec::new(),
            soroban_fee_write_1kb: 0,
        })
    }

    /// Create a snapshot of the current ledger state.
    ///
    /// The snapshot includes a lookup function for entries not in the cache,
    /// which queries the bucket list for the entry.
    pub fn create_snapshot(&self) -> Result<SnapshotHandle> {
        let state = self.state.read();
        // Use an empty entry cache - all lookups go through lookup_fn which handles:
        // - Soroban types (CONTRACT_DATA, CONTRACT_CODE, TTL): O(1) via in-memory soroban_state
        // - Classic types (accounts, trustlines, offers, etc.): O(log n) via bucket list snapshot
        // This avoids cloning up to 100k entries on every ledger, which was causing severe
        // performance degradation (45ms per ledger once cache filled).
        let entries = HashMap::new();

        let snapshot = LedgerSnapshot::new(state.header.clone(), state.header_hash, entries);

        // Create a lookup function that checks in-memory Soroban state first for O(1) access,
        // then falls back to a bucket list snapshot for non-Soroban types or cache misses.
        // This optimization provides O(1) lookups for CONTRACT_DATA, CONTRACT_CODE, and TTL
        // entries instead of O(log n) bucket list B-tree traversals.
        //
        // We capture a BucketListSnapshot instead of the live Arc<RwLock<BucketList>> so that
        // point lookups during TX execution don't contend with the write lock held during
        // commit() (add_batch + hash computation). The snapshot holds Arc<Bucket> references
        // which are cheap clones and require no locking.
        let soroban_state_lookup = self.soroban_state.clone();
        let bucket_list_snapshot = Arc::new({
            let bl = self.bucket_list.read();
            henyey_bucket::BucketListSnapshot::new(&bl, state.header.clone())
        });
        let bls_for_lookup = bucket_list_snapshot.clone();
        let lookup_fn: crate::snapshot::EntryLookupFn = Arc::new(move |key: &LedgerKey| {
            // For Soroban entry types, check in-memory state first (O(1)),
            // then fall back to bucket list if not found.
            if crate::soroban_state::InMemorySorobanState::is_in_memory_type(key) {
                if let Some(entry) = soroban_state_lookup.read().get(key) {
                    return Ok(Some((*entry).clone()));
                }
                // Fall through to bucket list for entries not in in-memory state
            }
            // Non-Soroban types or Soroban cache miss: use bucket list snapshot
            Ok(bls_for_lookup.get(key))
        });

        // Batch lookup function for loading multiple entries in a single pass.
        // Checks in-memory Soroban state first for ContractData/ContractCode/TTL/ConfigSetting
        // entries (O(1) cache hits), then falls back to the bucket list for any
        // Soroban entries not found in the in-memory state, then batch-loads
        // remaining non-Soroban keys from the bucket list in a single traversal.
        let soroban_state_batch = self.soroban_state.clone();
        let bls_for_batch = bucket_list_snapshot.clone();
        let batch_lookup_fn: crate::snapshot::BatchEntryLookupFn =
            Arc::new(move |keys: &[LedgerKey]| {
                let mut result = Vec::new();
                let mut bucket_list_keys = Vec::new();

                // Check soroban state cache for soroban types first (O(1))
                {
                    let soroban = soroban_state_batch.read();
                    for key in keys {
                        if crate::soroban_state::InMemorySorobanState::is_in_memory_type(key) {
                            if let Some(entry) = soroban.get(key) {
                                result.push((*entry).clone());
                            } else {
                                // Fall through to bucket list for Soroban entries not
                                // found in the in-memory state. This handles entries
                                // that the initialization scan may have missed.
                                bucket_list_keys.push(key.clone());
                            }
                            continue;
                        }
                        bucket_list_keys.push(key.clone());
                    }
                }

                // Batch-load remaining from bucket list in a single pass
                if !bucket_list_keys.is_empty() {
                    let bucket_entries = bls_for_batch
                        .load_keys_result(&bucket_list_keys)
                        .map_err(LedgerError::Bucket)?;
                    result.extend(bucket_entries);
                }

                Ok(result)
            });

        // Create entries function that reads from the in-memory offer store.
        // This avoids expensive SQL queries or bucket list scans during orderbook operations.
        // The in-memory store is populated at initialization and maintained incrementally.
        // Offers are always initialized before the first ledger close, so no fallback is needed.
        let offer_store = self.offer_store.clone();
        let entries_fn: crate::snapshot::EntriesLookupFn = Arc::new(move || {
            let store = offer_store.lock();
            Ok(store.all_ledger_entries())
        });

        // Create index-based lookup for offers by (account, asset).
        let offer_store_idx = self.offer_store.clone();
        let offers_by_account_asset_fn: crate::snapshot::OffersByAccountAssetFn = Arc::new(
            move |account_id: &AccountId, asset: &stellar_xdr::curr::Asset| {
                let store = offer_store_idx.lock();
                Ok(store.offers_by_account_and_asset_as_entries(account_id, asset))
            },
        );

        // Create index-based lookup for pool share trustlines by account.
        // This mirrors stellar-core's `getPoolShareTrustLine(accountID, asset)` which
        // queries SQL for all pool share trustlines owned by an account.  Without this
        // loader, `find_pool_share_trustlines_for_asset` would only find trustlines
        // already in the in-memory state, missing pool shares loaded from the bucket list.
        let pool_share_idx = self.pool_share_tl_account_index.clone();
        let pool_share_tls_by_account_fn: crate::snapshot::PoolShareTrustlinesByAccountFn =
            Arc::new(move |account_id| {
                let idx = pool_share_idx.read();
                Ok(idx
                    .get(account_id)
                    .map(|pool_ids| {
                        pool_ids
                            .iter()
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default())
            });

        let mut handle = SnapshotHandle::with_lookups_and_entries(snapshot, lookup_fn, entries_fn);
        handle.set_batch_lookup(batch_lookup_fn);
        handle.set_offers_by_account_asset(offers_by_account_asset_fn);
        handle.set_pool_share_tls_by_account(pool_share_tls_by_account_fn);
        Ok(handle)
    }

    /// Commit a ledger close.
    ///
    /// This is called by LedgerCloseContext::commit().
    fn commit_close(
        &self,
        offer_pool_changes: Vec<crate::delta::EntryChange>,
        new_header: LedgerHeader,
        new_header_hash: Hash256,
        has_offers: bool,
        has_pool_share_trustlines: bool,
    ) -> Result<()> {
        // Note: Bucket list was already updated in LedgerCloseContext::commit()
        // Just validate the hash if configured
        if self.config.validate_bucket_hash {
            let bucket_list = self.bucket_list.read();
            let live_hash = bucket_list.hash();

            // Compute bucket list hash based on protocol version.
            // For protocol >= 23, the hash is SHA256(live_hash || hot_archive_hash).
            // For earlier protocols, the hash is just the live bucket list hash.
            let computed =
                if protocol_version_starts_from(new_header.ledger_version, ProtocolVersion::V23) {
                    let hot_archive_guard = self.hot_archive_bucket_list.read();
                    if let Some(ref hot_archive) = *hot_archive_guard {
                        use sha2::{Digest, Sha256};
                        let hot_hash = hot_archive.hash();
                        let mut hasher = Sha256::new();
                        hasher.update(live_hash.as_bytes());
                        hasher.update(hot_hash.as_bytes());
                        let result = hasher.finalize();
                        let mut bytes = [0u8; 32];
                        bytes.copy_from_slice(&result);
                        Hash256::from_bytes(bytes)
                    } else {
                        tracing::warn!(
                            "Protocol >= 23 but no hot archive bucket list present, \
                         using live hash only - this WILL cause hash mismatch!"
                        );
                        live_hash
                    }
                } else {
                    live_hash
                };

            let expected = Hash256::from(new_header.bucket_list_hash.0);
            if computed != expected {
                return Err(LedgerError::HashMismatch {
                    expected: expected.to_hex(),
                    actual: computed.to_hex(),
                });
            }
        }

        // Update offer store and pool share index — skip iteration entirely if no relevant changes.
        if has_offers || has_pool_share_trustlines {
        let offers_initialized = has_offers && *self.offers_initialized.read();
        for change in &offer_pool_changes {
            // Quick discriminant check on the entry data to avoid expensive key construction
            let entry_ref = match change {
                EntryChange::Created(e) | EntryChange::Deleted { previous: e } => e,
                EntryChange::Updated { current, .. } => current,
            };
            match &entry_ref.data {
                LedgerEntryData::Offer(_) if offers_initialized => {
                    let mut store = self.offer_store.lock();
                    match change {
                        EntryChange::Created(entry) => {
                            store.insert_from_ledger_entry(&entry);
                        }
                        EntryChange::Updated { current, .. } => {
                            store.insert_from_ledger_entry(&current);
                        }
                        EntryChange::Deleted { previous } => {
                            if let LedgerEntryData::Offer(ref o) = previous.data {
                                store.remove_by_seller(&o.seller_id, o.offer_id);
                            }
                        }
                    }
                }
                LedgerEntryData::Trustline(tl) if matches!(tl.asset, stellar_xdr::curr::TrustLineAsset::PoolShare(_)) => {
                    match change {
                        EntryChange::Created(entry) => {
                            if let LedgerEntryData::Trustline(ref tl) = entry.data {
                                if let stellar_xdr::curr::TrustLineAsset::PoolShare(ref pool_id) = tl.asset {
                                    self.pool_share_tl_account_index
                                        .write()
                                        .entry(tl.account_id.clone())
                                        .or_default()
                                        .insert(pool_id.clone());
                                }
                            }
                        }
                        EntryChange::Deleted { previous } => {
                            if let LedgerEntryData::Trustline(ref tl) = previous.data {
                                if let stellar_xdr::curr::TrustLineAsset::PoolShare(ref pool_id) = tl.asset {
                                    let mut idx = self.pool_share_tl_account_index.write();
                                    if let Some(pools) = idx.get_mut(&tl.account_id) {
                                        pools.remove(pool_id);
                                    }
                                }
                            }
                        }
                        _ => {} // Updated pool share trustlines: no index change needed
                    }
                }
                _ => {} // Skip non-offer, non-pool-share entries
            }
        }
        } // end if has_offers || has_pool_share_trustlines

        // Update state
        {
            let mut state = self.state.write();
            state.header = new_header;
            state.header_hash = new_header_hash;
        }

        // Drop offer/pool changes on a background thread if non-trivial.
        if !offer_pool_changes.is_empty() {
            std::thread::spawn(move || drop(offer_pool_changes));
        }

        Ok(())
    }

    /// Build a memory report with per-component heap estimates.
    ///
    /// Acquires read locks on each component and calls estimate_heap_bytes().
    /// Total cost: <100μs.
    pub fn build_memory_report(&self, ledger_seq: u32) -> crate::memory_report::MemoryReport {
        use henyey_common::memory::ComponentMemory;

        let mut components = Vec::new();

        // Soroban state
        {
            let state = self.soroban_state.read();
            let data_bytes = state.estimate_contract_data_heap_bytes();
            let code_bytes = state.estimate_contract_code_heap_bytes();
            components.push(ComponentMemory::new(
                "soroban_data",
                data_bytes as u64,
                state.contract_data_count() as u64,
            ));
            components.push(ComponentMemory::new(
                "soroban_code",
                code_bytes as u64,
                state.contract_code_count() as u64,
            ));
        }

        // Unified offer store (canonical data + all indexes)
        {
            let store = self.offer_store.lock();
            let offer_bytes = store.estimate_heap_bytes();
            let offer_count = store.len();
            components.push(ComponentMemory::new(
                "offers",
                offer_bytes as u64,
                offer_count as u64,
            ));
        }

        // Bucket list
        {
            let bl = self.bucket_list.read();
            let heap_bytes = bl.estimate_heap_bytes();
            let mmap = bl.mmap_bytes();
            let cache = bl.cache_bytes();
            components.push(ComponentMemory::new(
                "bucket_list_heap",
                heap_bytes as u64,
                0,
            ));
            components.push(ComponentMemory::new_non_heap(
                "bucket_list_mmap",
                mmap as u64,
                0,
            ));
            components.push(ComponentMemory::new(
                "bucket_list_cache",
                cache as u64,
                0,
            ));
        }

        // Module cache
        {
            let mc = self.module_cache.read();
            if mc.is_some() {
                // Heuristic: compiled modules are ~4x their WASM size
                let soroban = self.soroban_state.read();
                let wasm_bytes = soroban.contract_code_state_size().max(0) as u64;
                let code_count = soroban.contract_code_count() as u64;
                let estimated_compiled = wasm_bytes * 4;
                components.push(ComponentMemory::new(
                    "module_cache",
                    estimated_compiled,
                    code_count,
                ));
            }
        }

        // Transaction executor state (LedgerStateManager with offers, accounts, etc.)
        {
            let executor_guard = self.executor.lock();
            if let Some(ref executor) = *executor_guard {
                let (total_bytes, offer_bytes) = executor.state().estimate_heap_bytes();
                let offer_count = executor.state().offer_count() as u64;
                components.push(ComponentMemory::new(
                    "executor_state",
                    total_bytes as u64,
                    0,
                ));
                components.push(ComponentMemory::new(
                    "executor_offers",
                    offer_bytes as u64,
                    offer_count,
                ));
            }
        }

        // Hot archive bucket list
        {
            let ha = self.hot_archive_bucket_list.read();
            if let Some(ref hot_archive) = *ha {
                let heap = hot_archive.estimate_heap_bytes();
                let mmap = hot_archive.mmap_bytes();
                components.push(ComponentMemory::new(
                    "hot_archive_heap",
                    heap as u64,
                    0,
                ));
                components.push(ComponentMemory::new_non_heap(
                    "hot_archive_mmap",
                    mmap as u64,
                    0,
                ));
            }
        }

        crate::memory_report::MemoryReport::new(ledger_seq, components)
    }

    /// Get Soroban network configuration information.
    ///
    /// Returns the Soroban-related configuration settings from the current ledger
    /// state, or `None` if not available (pre-protocol 20 or not initialized).
    /// Inject a synthetic CONTRACT_DATA entry directly into the in-memory Soroban state.
    ///
    /// This is intended for test/benchmark harnesses (e.g. ApplyLoad) that need to
    /// inject data (such as a ConfigUpgradeSet) into the ledger without going through
    /// a Soroban transaction. The entry and its TTL become visible to subsequent
    /// `create_snapshot()` calls.
    pub fn inject_synthetic_contract_data(
        &self,
        entry: LedgerEntry,
        live_until_ledger: u32,
    ) -> Result<()> {
        use stellar_xdr::curr::LedgerEntryData;

        // Verify it's a CONTRACT_DATA entry
        let _cd = match &entry.data {
            LedgerEntryData::ContractData(cd) => cd,
            _ => {
                return Err(LedgerError::Internal(
                    "inject_synthetic_contract_data: not a CONTRACT_DATA entry".to_string(),
                ))
            }
        };

        let last_modified = entry.last_modified_ledger_seq;
        let ttl_data = crate::soroban_state::TtlData::new(live_until_ledger, last_modified);

        // Build the LedgerKey for TTL
        let data_key = LedgerKey::ContractData(stellar_xdr::curr::LedgerKeyContractData {
            contract: _cd.contract.clone(),
            key: _cd.key.clone(),
            durability: _cd.durability,
        });
        let key_hash = Hash256::hash_xdr(&data_key).map_err(|e| {
            LedgerError::Internal(format!("Failed to hash LedgerKey: {}", e))
        })?;
        let ttl_key = stellar_xdr::curr::LedgerKeyTtl {
            key_hash: stellar_xdr::curr::Hash(key_hash.0),
        };

        let mut state = self.soroban_state.write();
        state.create_contract_data(entry)?;
        state.create_ttl(&ttl_key, ttl_data)?;

        Ok(())
    }

    pub fn soroban_network_info(&self) -> Option<SorobanNetworkInfo> {
        if !self.is_initialized() {
            return None;
        }
        let snapshot = self.create_snapshot().ok()?;
        load_soroban_network_info(&snapshot)
    }

    /// Rebuild the module cache for a new protocol version.
    ///
    /// When a protocol upgrade changes the version (e.g., P24→P25), the module
    /// cache must be rebuilt for the new protocol. This scans the bucket list
    /// for all CONTRACT_CODE entries and compiles them into a new cache.
    ///
    /// Parity: In stellar-core, the module cache is pre-compiled for all
    /// supported protocol versions simultaneously. Since Henyey only maintains
    /// a single-protocol cache, we rebuild it on protocol upgrade instead.
    fn rebuild_module_cache(&self, protocol_version: u32) {
        let start = std::time::Instant::now();

        let new_cache = match PersistentModuleCache::new_for_protocol(protocol_version) {
            Some(cache) => cache,
            None => {
                tracing::warn!(
                    protocol_version,
                    "Failed to create module cache for new protocol version"
                );
                return;
            }
        };

        let bucket_list = self.bucket_list.read();
        let mut compiled = 0u32;
        let mut seen_hashes = std::collections::HashSet::<stellar_xdr::curr::Hash>::new();

        // Scan levels from 0 (newest) to 10 (oldest). Within each level,
        // curr shadows snap. Dead entries shadow live entries at higher levels.
        for level in bucket_list.levels() {
            for bucket in [level.curr.as_ref(), level.snap.as_ref()] {
                for entry in bucket.iter() {
                    match &entry {
                        henyey_bucket::BucketEntry::Liveentry(le)
                        | henyey_bucket::BucketEntry::Initentry(le) => {
                            if let LedgerEntryData::ContractCode(ref cc) = le.data {
                                let hash = stellar_xdr::curr::Hash(
                                    <sha2::Sha256 as sha2::Digest>::digest(cc.code.as_slice())
                                        .into(),
                                );
                                if seen_hashes.insert(hash) {
                                    if new_cache
                                        .add_contract(cc.code.as_slice(), protocol_version)
                                    {
                                        compiled += 1;
                                    }
                                }
                            }
                        }
                        henyey_bucket::BucketEntry::Deadentry(_)
                        | henyey_bucket::BucketEntry::Metaentry(_) => {}
                    }
                }
            }
        }
        drop(bucket_list);

        *self.module_cache.write() = Some(new_cache);

        let elapsed = start.elapsed();
        tracing::info!(
            protocol_version,
            compiled,
            elapsed_ms = elapsed.as_millis() as u64,
            "Rebuilt module cache for protocol upgrade"
        );
    }

    /// Look up a pending ConfigUpgradeSet by its key.
    ///
    /// This retrieves a ConfigUpgradeSet that has been uploaded to the network
    /// but not yet applied. Validators use this to validate scheduled upgrades.
    ///
    /// Returns `None` if:
    /// - The ledger is not initialized
    /// - The CONTRACT_DATA entry doesn't exist
    /// - The entry's TTL has expired
    /// - The entry is not TEMPORARY durability
    /// - The XDR cannot be decoded
    pub fn get_config_upgrade_set(
        &self,
        key: &stellar_xdr::curr::ConfigUpgradeSetKey,
    ) -> Option<std::sync::Arc<crate::config_upgrade::ConfigUpgradeSetFrame>> {
        if !self.is_initialized() {
            return None;
        }
        let snapshot = self.create_snapshot().ok()?;
        crate::config_upgrade::ConfigUpgradeSetFrame::make_from_key(&snapshot, key)
    }
}

/// Internal context for closing a single ledger.
///
/// This struct is used internally by [`LedgerManager::close_ledger`] to
/// process transactions and finalize the ledger.
struct LedgerCloseContext<'a> {
    manager: &'a LedgerManager,
    close_data: LedgerCloseData,
    prev_header: LedgerHeader,
    prev_header_hash: Hash256,
    delta: LedgerDelta,
    snapshot: SnapshotHandle,
    stats: LedgerCloseStats,
    upgrade_ctx: UpgradeContext,
    id_pool: u64,
    tx_results: Vec<stellar_xdr::curr::TransactionResultPair>,
    tx_result_metas: Vec<stellar_xdr::curr::TransactionResultMetaV1>,
    /// Keys of entries restored from hot archive during transaction execution.
    /// Passed to HotArchiveBucketList::add_batch to remove restored entries from archive.
    hot_archive_restored_keys: Vec<LedgerKey>,
    /// Tokio runtime handle for spawning parallel work from non-worker threads.
    runtime_handle: Option<tokio::runtime::Handle>,
    /// Timer started at `begin_close()` to measure the full ledger close lifecycle.
    start: std::time::Instant,
    // Phase timing fields (microseconds), populated by close_ledger() and commit().
    timing_begin_close_us: u64,
    timing_tx_exec_us: u64,
    timing_classic_exec_us: u64,
    timing_soroban_exec_us: u64,
    // Sub-phase timings from apply_transactions (microseconds).
    timing_prepare_us: u64,
    timing_config_load_us: u64,
    timing_executor_setup_us: u64,
    timing_fee_pre_deduct_us: u64,
    timing_post_exec_us: u64,
    /// Per-transaction execution timing and metadata for perf reporting.
    tx_perf: Vec<crate::close::TxPerf>,
    /// Soroban fee per 1KB write (rent fee), cached from SorobanConfig for meta ext V1.
    /// Set during close_ledger() when SorobanConfig is loaded.
    soroban_fee_write_1kb: i64,
}

impl<'a> LedgerCloseContext<'a> {
    /// Load an entry from the snapshot.
    fn load_entry(&self, key: &LedgerKey) -> Result<Option<LedgerEntry>> {
        // First check if we have a pending change
        if let Some(change) = self.delta.get_change(key) {
            return Ok(change.current_entry().cloned());
        }

        // Otherwise read from snapshot
        self.snapshot.get_entry(key)
    }

    /// Load StateArchivalSettings from the delta (for upgraded values) falling back to snapshot.
    ///
    /// Parity: In stellar-core, the eviction scan runs after config upgrades are applied to the
    /// LedgerTxn, so it sees the upgraded StateArchival settings. We must do the same
    /// by checking the delta first (which contains the upgrade), then falling back to
    /// the snapshot.
    fn load_state_archival_settings(&self) -> Option<StateArchivalSettings> {
        let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::StateArchival,
        });
        let entry = self.load_entry(&key).ok()??;
        if let LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(settings)) =
            entry.data
        {
            Some(settings)
        } else {
            None
        }
    }

    /// Create the initial Soroban configuration entries for protocol v20.
    ///
    /// Parity: NetworkConfig.cpp:1388-1430 `createLedgerEntriesForV20`
    /// Creates 14 CONFIG_SETTING ledger entries with initial values for Soroban.
    /// This is called when the network upgrades from pre-Soroban (< v20) to v20+.
    fn create_ledger_entries_for_v20(&mut self) -> Result<()> {
        use stellar_xdr::curr::{
            ConfigSettingContractBandwidthV0, ConfigSettingContractComputeV0,
            ConfigSettingContractEventsV0, ConfigSettingContractExecutionLanesV0,
            ConfigSettingContractHistoricalDataV0, ConfigSettingContractLedgerCostV0,
            ContractCostParams, StateArchivalSettings,
        };

        let ledger_seq = self.close_data.ledger_seq;
        let make_entry = |config: ConfigSettingEntry| -> LedgerEntry {
            LedgerEntry {
                last_modified_ledger_seq: ledger_seq,
                data: LedgerEntryData::ConfigSetting(config),
                ext: LedgerEntryExt::V0,
            }
        };

        // 1. CONFIG_SETTING_CONTRACT_MAX_SIZE_BYTES
        // Parity: NetworkConfig.cpp:44-58 initialMaxContractSizeEntry
        // MinimumSorobanNetworkConfig::MAX_CONTRACT_SIZE = 2000
        self.delta.record_create(make_entry(
            ConfigSettingEntry::ContractMaxSizeBytes(2_000),
        ))?;

        // 2. CONFIG_SETTING_CONTRACT_DATA_KEY_SIZE_BYTES
        // Parity: NetworkConfig.cpp:60-74 initialMaxContractDataKeySizeEntry
        // MinimumSorobanNetworkConfig::MAX_CONTRACT_DATA_KEY_SIZE_BYTES = 200
        self.delta.record_create(make_entry(
            ConfigSettingEntry::ContractDataKeySizeBytes(200),
        ))?;

        // 3. CONFIG_SETTING_CONTRACT_DATA_ENTRY_SIZE_BYTES
        // Parity: NetworkConfig.cpp:76-90 initialMaxContractDataEntrySizeEntry
        // MinimumSorobanNetworkConfig::MAX_CONTRACT_DATA_ENTRY_SIZE_BYTES = 2000
        self.delta.record_create(make_entry(
            ConfigSettingEntry::ContractDataEntrySizeBytes(2_000),
        ))?;

        // 4. CONFIG_SETTING_CONTRACT_COMPUTE_V0
        // Parity: NetworkConfig.cpp:92-116 initialContractComputeSettingsEntry
        self.delta.record_create(make_entry(
            ConfigSettingEntry::ContractComputeV0(ConfigSettingContractComputeV0 {
                // TX_MAX_INSTRUCTIONS = MinimumSorobanNetworkConfig::TX_MAX_INSTRUCTIONS = 2_500_000
                ledger_max_instructions: 2_500_000, // LEDGER_MAX_INSTRUCTIONS = TX_MAX_INSTRUCTIONS
                tx_max_instructions: 2_500_000,
                fee_rate_per_instructions_increment: 100,
                tx_memory_limit: 2_000_000, // MEMORY_LIMIT = MinimumSorobanNetworkConfig::MEMORY_LIMIT
            }),
        ))?;

        // 5. CONFIG_SETTING_CONTRACT_LEDGER_COST_V0
        // Parity: NetworkConfig.cpp:118-175 initialContractLedgerAccessSettingsEntry
        self.delta.record_create(make_entry(
            ConfigSettingEntry::ContractLedgerCostV0(ConfigSettingContractLedgerCostV0 {
                ledger_max_disk_read_entries: 3,      // LEDGER_MAX_READ_LEDGER_ENTRIES = TX_MAX
                ledger_max_disk_read_bytes: 3_200,    // LEDGER_MAX_READ_BYTES = TX_MAX
                ledger_max_write_ledger_entries: 2,    // TX_MAX_WRITE_LEDGER_ENTRIES
                ledger_max_write_bytes: 3_200,         // TX_MAX_WRITE_BYTES
                tx_max_disk_read_entries: 3,
                tx_max_disk_read_bytes: 3_200,
                tx_max_write_ledger_entries: 2,
                tx_max_write_bytes: 3_200,
                fee_disk_read_ledger_entry: 5_000,
                fee_write_ledger_entry: 20_000,
                fee_disk_read1_kb: 1_000,
                soroban_state_target_size_bytes: 30 * 1024 * 1024 * 1024_i64, // 30 GB
                rent_fee1_kb_soroban_state_size_low: 1_000,
                rent_fee1_kb_soroban_state_size_high: 10_000,
                soroban_state_rent_fee_growth_factor: 1,
            }),
        ))?;

        // 6. CONFIG_SETTING_CONTRACT_HISTORICAL_DATA_V0
        // Parity: NetworkConfig.cpp:177-186 initialContractHistoricalDataSettingsEntry
        self.delta.record_create(make_entry(
            ConfigSettingEntry::ContractHistoricalDataV0(ConfigSettingContractHistoricalDataV0 {
                fee_historical1_kb: 100,
            }),
        ))?;

        // 7. CONFIG_SETTING_CONTRACT_EVENTS_V0
        // Parity: NetworkConfig.cpp:188-205 initialContractEventsSettingsEntry
        self.delta.record_create(make_entry(
            ConfigSettingEntry::ContractEventsV0(ConfigSettingContractEventsV0 {
                tx_max_contract_events_size_bytes: 200, // MinimumSorobanNetworkConfig
                fee_contract_events1_kb: 200,
            }),
        ))?;

        // 8. CONFIG_SETTING_CONTRACT_BANDWIDTH_V0
        // Parity: NetworkConfig.cpp:207-227 initialContractBandwidthSettingsEntry
        self.delta.record_create(make_entry(
            ConfigSettingEntry::ContractBandwidthV0(ConfigSettingContractBandwidthV0 {
                ledger_max_txs_size_bytes: 10_000, // TX_MAX_SIZE_BYTES = LEDGER_MAX
                tx_max_size_bytes: 10_000,
                fee_tx_size1_kb: 2_000,
            }),
        ))?;

        // 9. CONFIG_SETTING_CONTRACT_EXECUTION_LANES
        // Parity: NetworkConfig.cpp:229-243 initialContractExecutionLanesSettingsEntry
        self.delta.record_create(make_entry(
            ConfigSettingEntry::ContractExecutionLanes(ConfigSettingContractExecutionLanesV0 {
                ledger_max_tx_count: 1,
            }),
        ))?;

        // 10. CONFIG_SETTING_CONTRACT_COST_PARAMS_CPU_INSTRUCTIONS
        // Parity: NetworkConfig.cpp:246-338 initialCpuCostParamsEntryForV20
        let cpu_params = Self::initial_cpu_cost_params_for_v20();
        self.delta.record_create(make_entry(
            ConfigSettingEntry::ContractCostParamsCpuInstructions(ContractCostParams(
                cpu_params.try_into().map_err(|_| {
                    LedgerError::Internal("Failed to create V20 CPU cost params".to_string())
                })?,
            )),
        ))?;

        // 11. CONFIG_SETTING_CONTRACT_COST_PARAMS_MEMORY_BYTES
        // Parity: NetworkConfig.cpp:688-776 initialMemCostParamsEntryForV20
        let mem_params = Self::initial_mem_cost_params_for_v20();
        self.delta.record_create(make_entry(
            ConfigSettingEntry::ContractCostParamsMemoryBytes(ContractCostParams(
                mem_params.try_into().map_err(|_| {
                    LedgerError::Internal("Failed to create V20 memory cost params".to_string())
                })?,
            )),
        ))?;

        // 12. CONFIG_SETTING_STATE_ARCHIVAL
        // Parity: NetworkConfig.cpp:632-685 initialStateArchivalSettings
        self.delta.record_create(make_entry(
            ConfigSettingEntry::StateArchival(StateArchivalSettings {
                max_entry_ttl: 1_054_080,                   // MAXIMUM_ENTRY_LIFETIME (61 days)
                min_persistent_ttl: 4_096,                  // Live until level 6
                min_temporary_ttl: 16,
                persistent_rent_rate_denominator: 252_480,
                temp_rent_rate_denominator: 2_524_800,
                max_entries_to_archive: 100,
                live_soroban_state_size_window_sample_size: 30,
                live_soroban_state_size_window_sample_period: 64,
                eviction_scan_size: 100_000,                // 100 kb
                starting_eviction_scan_level: 6,
            }),
        ))?;

        // 13. CONFIG_SETTING_LIVE_SOROBAN_STATE_SIZE_WINDOW
        // Parity: NetworkConfig.cpp:1110-1126 initialliveSorobanStateSizeWindow
        // Populates 30-entry window with copies of current bucket list size.
        let bl_size = self.manager.bucket_list.read().sum_bucket_entry_counters().total_size();
        let window: Vec<u64> = vec![bl_size; 30]; // BUCKET_LIST_SIZE_WINDOW_SAMPLE_SIZE = 30
        self.delta.record_create(make_entry(
            ConfigSettingEntry::LiveSorobanStateSizeWindow(
                window.try_into().map_err(|_| {
                    LedgerError::Internal(
                        "Failed to create state size window".to_string(),
                    )
                })?,
            ),
        ))?;

        // 14. CONFIG_SETTING_EVICTION_ITERATOR
        // Parity: NetworkConfig.cpp:1128-1139 initialEvictionIterator
        self.delta.record_create(make_entry(
            ConfigSettingEntry::EvictionIterator(EvictionIterator {
                bucket_list_level: 6, // STARTING_EVICTION_SCAN_LEVEL
                is_curr_bucket: true,
                bucket_file_offset: 0,
            }),
        ))?;

        tracing::info!(
            ledger_seq = self.close_data.ledger_seq,
            "Applied createLedgerEntriesForV20: created 14 CONFIG_SETTING entries"
        );

        Ok(())
    }

    /// Build the initial V20 CPU cost parameter table (23 entries: indices 0..=22).
    ///
    /// Parity: NetworkConfig.cpp:246-338 `initialCpuCostParamsEntryForV20`
    fn initial_cpu_cost_params_for_v20() -> Vec<stellar_xdr::curr::ContractCostParamEntry> {
        use stellar_xdr::curr::{ContractCostParamEntry, ExtensionPoint};
        let e = |const_term: i64, linear_term: i64| ContractCostParamEntry {
            ext: ExtensionPoint::V0,
            const_term,
            linear_term,
        };
        // Indices 0..=22 (ChaCha20DrawBytes)
        vec![
            e(4, 0),           // 0: WasmInsnExec
            e(434, 16),        // 1: MemAlloc
            e(42, 16),         // 2: MemCpy
            e(44, 16),         // 3: MemCmp
            e(310, 0),         // 4: DispatchHostFunction
            e(61, 0),          // 5: VisitObject
            e(230, 29),        // 6: ValSer
            e(59052, 4001),    // 7: ValDeser
            e(3738, 7012),     // 8: ComputeSha256Hash
            e(40253, 0),       // 9: ComputeEd25519PubKey
            e(377524, 4068),   // 10: VerifyEd25519Sig
            e(451626, 45405),  // 11: VmInstantiation
            e(451626, 45405),  // 12: VmCachedInstantiation
            e(1948, 0),        // 13: InvokeVmFunction
            e(3766, 5969),     // 14: ComputeKeccak256Hash
            e(710, 0),         // 15: DecodeEcdsaCurve256Sig
            e(2315295, 0),     // 16: RecoverEcdsaSecp256k1Key
            e(4404, 0),        // 17: Int256AddSub
            e(4947, 0),        // 18: Int256Mul
            e(4911, 0),        // 19: Int256Div
            e(4286, 0),        // 20: Int256Pow
            e(913, 0),         // 21: Int256Shift
            e(1058, 501),      // 22: ChaCha20DrawBytes
        ]
    }

    /// Build the initial V20 memory cost parameter table (23 entries: indices 0..=22).
    ///
    /// Parity: NetworkConfig.cpp:688-776 `initialMemCostParamsEntryForV20`
    fn initial_mem_cost_params_for_v20() -> Vec<stellar_xdr::curr::ContractCostParamEntry> {
        use stellar_xdr::curr::{ContractCostParamEntry, ExtensionPoint};
        let e = |const_term: i64, linear_term: i64| ContractCostParamEntry {
            ext: ExtensionPoint::V0,
            const_term,
            linear_term,
        };
        // Indices 0..=22 (ChaCha20DrawBytes)
        vec![
            e(0, 0),           // 0: WasmInsnExec
            e(16, 128),        // 1: MemAlloc
            e(0, 0),           // 2: MemCpy
            e(0, 0),           // 3: MemCmp
            e(0, 0),           // 4: DispatchHostFunction
            e(0, 0),           // 5: VisitObject
            e(242, 384),       // 6: ValSer
            e(0, 384),         // 7: ValDeser
            e(0, 0),           // 8: ComputeSha256Hash
            e(0, 0),           // 9: ComputeEd25519PubKey
            e(0, 0),           // 10: VerifyEd25519Sig
            e(130065, 5064),   // 11: VmInstantiation
            e(130065, 5064),   // 12: VmCachedInstantiation
            e(14, 0),          // 13: InvokeVmFunction
            e(0, 0),           // 14: ComputeKeccak256Hash
            e(0, 0),           // 15: DecodeEcdsaCurve256Sig
            e(181, 0),         // 16: RecoverEcdsaSecp256k1Key
            e(99, 0),          // 17: Int256AddSub
            e(99, 0),          // 18: Int256Mul
            e(99, 0),          // 19: Int256Div
            e(99, 0),          // 20: Int256Pow
            e(99, 0),          // 21: Int256Shift
            e(0, 0),           // 22: ChaCha20DrawBytes
        ]
    }

    /// Apply version upgrade side effects for protocol 21.
    ///
    /// Parity: NetworkConfig.cpp:1432-1439 `createCostTypesForV21`
    /// Resizes CPU and memory cost params to include ParseWasm/InstantiateWasm types
    /// and updates VmCachedInstantiation.
    fn create_cost_types_for_v21(&mut self) -> Result<()> {
        use stellar_xdr::curr::{ContractCostParamEntry, ContractCostParams, ExtensionPoint};

        // V21 last cost type: VerifyEcdsaSecp256r1Sig = 44
        const NEW_SIZE: usize = 45;

        let make_entry = |const_term: i64, linear_term: i64| ContractCostParamEntry {
            ext: ExtensionPoint::V0,
            const_term,
            linear_term,
        };

        // --- Update CPU cost params ---
        // Parity: NetworkConfig.cpp:340-441 updateCpuCostParamsEntryForV21
        let cpu_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractCostParamsCpuInstructions,
        });
        let cpu_entry = self.load_entry(&cpu_key)?.ok_or_else(|| {
            LedgerError::Internal("ContractCostParamsCpuInstructions entry not found".to_string())
        })?;
        let mut cpu_params = if let LedgerEntryData::ConfigSetting(
            ConfigSettingEntry::ContractCostParamsCpuInstructions(params),
        ) = &cpu_entry.data
        {
            params.0.to_vec()
        } else {
            return Err(LedgerError::Internal(
                "Unexpected entry type for CPU cost params".to_string(),
            ));
        };

        cpu_params.resize(NEW_SIZE, make_entry(0, 0));

        // Updated existing entry:
        cpu_params[12] = make_entry(41142, 634);          // VmCachedInstantiation
        // New entries (indices 23..=44):
        cpu_params[23] = make_entry(73077, 25410);         // ParseWasmInstructions
        cpu_params[24] = make_entry(0, 540752);            // ParseWasmFunctions
        cpu_params[25] = make_entry(0, 176363);            // ParseWasmGlobals
        cpu_params[26] = make_entry(0, 29989);             // ParseWasmTableEntries
        cpu_params[27] = make_entry(0, 1061449);           // ParseWasmTypes
        cpu_params[28] = make_entry(0, 237336);            // ParseWasmDataSegments
        cpu_params[29] = make_entry(0, 328476);            // ParseWasmElemSegments
        cpu_params[30] = make_entry(0, 701845);            // ParseWasmImports
        cpu_params[31] = make_entry(0, 429383);            // ParseWasmExports
        cpu_params[32] = make_entry(0, 28);                // ParseWasmDataSegmentBytes
        cpu_params[33] = make_entry(43030, 0);             // InstantiateWasmInstructions
        cpu_params[34] = make_entry(0, 7556);              // InstantiateWasmFunctions
        cpu_params[35] = make_entry(0, 10711);             // InstantiateWasmGlobals
        cpu_params[36] = make_entry(0, 3300);              // InstantiateWasmTableEntries
        cpu_params[37] = make_entry(0, 0);                 // InstantiateWasmTypes
        cpu_params[38] = make_entry(0, 23038);             // InstantiateWasmDataSegments
        cpu_params[39] = make_entry(0, 42488);             // InstantiateWasmElemSegments
        cpu_params[40] = make_entry(0, 828974);            // InstantiateWasmImports
        cpu_params[41] = make_entry(0, 297100);            // InstantiateWasmExports
        cpu_params[42] = make_entry(0, 14);                // InstantiateWasmDataSegmentBytes
        cpu_params[43] = make_entry(1882, 0);              // Sec1DecodePointUncompressed
        cpu_params[44] = make_entry(3000906, 0);           // VerifyEcdsaSecp256r1Sig

        let new_cpu_entry = LedgerEntry {
            last_modified_ledger_seq: self.close_data.ledger_seq,
            data: LedgerEntryData::ConfigSetting(
                ConfigSettingEntry::ContractCostParamsCpuInstructions(ContractCostParams(
                    cpu_params.try_into().map_err(|_| {
                        LedgerError::Internal("Failed to convert V21 CPU cost params".to_string())
                    })?,
                )),
            ),
            ext: LedgerEntryExt::V0,
        };
        self.delta.record_update(cpu_entry, new_cpu_entry)?;

        // --- Update memory cost params ---
        // Parity: NetworkConfig.cpp:778-880 updateMemCostParamsEntryForV21
        let mem_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractCostParamsMemoryBytes,
        });
        let mem_entry = self.load_entry(&mem_key)?.ok_or_else(|| {
            LedgerError::Internal("ContractCostParamsMemoryBytes entry not found".to_string())
        })?;
        let mut mem_params = if let LedgerEntryData::ConfigSetting(
            ConfigSettingEntry::ContractCostParamsMemoryBytes(params),
        ) = &mem_entry.data
        {
            params.0.to_vec()
        } else {
            return Err(LedgerError::Internal(
                "Unexpected entry type for memory cost params".to_string(),
            ));
        };

        mem_params.resize(NEW_SIZE, make_entry(0, 0));

        // Updated existing entry:
        mem_params[12] = make_entry(69472, 1217);          // VmCachedInstantiation
        // New entries (indices 23..=44):
        mem_params[23] = make_entry(17564, 6457);          // ParseWasmInstructions
        mem_params[24] = make_entry(0, 47464);             // ParseWasmFunctions
        mem_params[25] = make_entry(0, 13420);             // ParseWasmGlobals
        mem_params[26] = make_entry(0, 6285);              // ParseWasmTableEntries
        mem_params[27] = make_entry(0, 64670);             // ParseWasmTypes
        mem_params[28] = make_entry(0, 29074);             // ParseWasmDataSegments
        mem_params[29] = make_entry(0, 48095);             // ParseWasmElemSegments
        mem_params[30] = make_entry(0, 103229);            // ParseWasmImports
        mem_params[31] = make_entry(0, 36394);             // ParseWasmExports
        mem_params[32] = make_entry(0, 257);               // ParseWasmDataSegmentBytes
        mem_params[33] = make_entry(70704, 0);             // InstantiateWasmInstructions
        mem_params[34] = make_entry(0, 14613);             // InstantiateWasmFunctions
        mem_params[35] = make_entry(0, 6833);              // InstantiateWasmGlobals
        mem_params[36] = make_entry(0, 1025);              // InstantiateWasmTableEntries
        mem_params[37] = make_entry(0, 0);                 // InstantiateWasmTypes
        mem_params[38] = make_entry(0, 129632);            // InstantiateWasmDataSegments
        mem_params[39] = make_entry(0, 13665);             // InstantiateWasmElemSegments
        mem_params[40] = make_entry(0, 97637);             // InstantiateWasmImports
        mem_params[41] = make_entry(0, 9176);              // InstantiateWasmExports
        mem_params[42] = make_entry(0, 126);               // InstantiateWasmDataSegmentBytes
        mem_params[43] = make_entry(0, 0);                 // Sec1DecodePointUncompressed
        mem_params[44] = make_entry(0, 0);                 // VerifyEcdsaSecp256r1Sig

        let new_mem_entry = LedgerEntry {
            last_modified_ledger_seq: self.close_data.ledger_seq,
            data: LedgerEntryData::ConfigSetting(
                ConfigSettingEntry::ContractCostParamsMemoryBytes(ContractCostParams(
                    mem_params.try_into().map_err(|_| {
                        LedgerError::Internal("Failed to convert V21 memory cost params".to_string())
                    })?,
                )),
            ),
            ext: LedgerEntryExt::V0,
        };
        self.delta.record_update(mem_entry, new_mem_entry)?;

        tracing::info!(
            ledger_seq = self.close_data.ledger_seq,
            new_size = NEW_SIZE,
            "Applied createCostTypesForV21: resized cost params with ParseWasm/InstantiateWasm entries"
        );

        Ok(())
    }

    /// Apply version upgrade side effects for protocol 22.
    ///
    /// Parity: NetworkConfig.cpp:1441-1448 `createCostTypesForV22`
    /// Resizes CPU and memory cost params to include BLS12-381 curve types.
    fn create_cost_types_for_v22(&mut self) -> Result<()> {
        use stellar_xdr::curr::{ContractCostParamEntry, ContractCostParams, ExtensionPoint};

        // V22 last cost type: Bls12381FrInv = 69
        const NEW_SIZE: usize = 70;

        let make_entry = |const_term: i64, linear_term: i64| ContractCostParamEntry {
            ext: ExtensionPoint::V0,
            const_term,
            linear_term,
        };

        // --- Update CPU cost params ---
        // Parity: NetworkConfig.cpp:443-553 updateCpuCostParamsEntryForV22
        let cpu_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractCostParamsCpuInstructions,
        });
        let cpu_entry = self.load_entry(&cpu_key)?.ok_or_else(|| {
            LedgerError::Internal("ContractCostParamsCpuInstructions entry not found".to_string())
        })?;
        let mut cpu_params = if let LedgerEntryData::ConfigSetting(
            ConfigSettingEntry::ContractCostParamsCpuInstructions(params),
        ) = &cpu_entry.data
        {
            params.0.to_vec()
        } else {
            return Err(LedgerError::Internal(
                "Unexpected entry type for CPU cost params".to_string(),
            ));
        };

        cpu_params.resize(NEW_SIZE, make_entry(0, 0));

        // New BLS12-381 entries (indices 45..=69):
        cpu_params[45] = make_entry(661, 0);               // Bls12381EncodeFp
        cpu_params[46] = make_entry(985, 0);               // Bls12381DecodeFp
        cpu_params[47] = make_entry(1934, 0);              // Bls12381G1CheckPointOnCurve
        cpu_params[48] = make_entry(730510, 0);            // Bls12381G1CheckPointInSubgroup
        cpu_params[49] = make_entry(5921, 0);              // Bls12381G2CheckPointOnCurve
        cpu_params[50] = make_entry(1057822, 0);           // Bls12381G2CheckPointInSubgroup
        cpu_params[51] = make_entry(92642, 0);             // Bls12381G1ProjectiveToAffine
        cpu_params[52] = make_entry(100742, 0);            // Bls12381G2ProjectiveToAffine
        cpu_params[53] = make_entry(7689, 0);              // Bls12381G1Add
        cpu_params[54] = make_entry(2458985, 0);           // Bls12381G1Mul
        cpu_params[55] = make_entry(2426722, 96397671);    // Bls12381G1Msm
        cpu_params[56] = make_entry(1541554, 0);           // Bls12381MapFpToG1
        cpu_params[57] = make_entry(3211191, 6713);        // Bls12381HashToG1
        cpu_params[58] = make_entry(25207, 0);             // Bls12381G2Add
        cpu_params[59] = make_entry(7873219, 0);           // Bls12381G2Mul
        cpu_params[60] = make_entry(8035968, 309667335);   // Bls12381G2Msm
        cpu_params[61] = make_entry(2420202, 0);           // Bls12381MapFp2ToG2
        cpu_params[62] = make_entry(7050564, 6797);        // Bls12381HashToG2
        cpu_params[63] = make_entry(10558948, 632860943);  // Bls12381Pairing
        cpu_params[64] = make_entry(1994, 0);              // Bls12381FrFromU256
        cpu_params[65] = make_entry(1155, 0);              // Bls12381FrToU256
        cpu_params[66] = make_entry(74, 0);                // Bls12381FrAddSub
        cpu_params[67] = make_entry(332, 0);               // Bls12381FrMul
        cpu_params[68] = make_entry(691, 74558);           // Bls12381FrPow
        cpu_params[69] = make_entry(35421, 0);             // Bls12381FrInv

        let new_cpu_entry = LedgerEntry {
            last_modified_ledger_seq: self.close_data.ledger_seq,
            data: LedgerEntryData::ConfigSetting(
                ConfigSettingEntry::ContractCostParamsCpuInstructions(ContractCostParams(
                    cpu_params.try_into().map_err(|_| {
                        LedgerError::Internal("Failed to convert V22 CPU cost params".to_string())
                    })?,
                )),
            ),
            ext: LedgerEntryExt::V0,
        };
        self.delta.record_update(cpu_entry, new_cpu_entry)?;

        // --- Update memory cost params ---
        // Parity: NetworkConfig.cpp:882-990 updateMemCostParamsEntryForV22
        let mem_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractCostParamsMemoryBytes,
        });
        let mem_entry = self.load_entry(&mem_key)?.ok_or_else(|| {
            LedgerError::Internal("ContractCostParamsMemoryBytes entry not found".to_string())
        })?;
        let mut mem_params = if let LedgerEntryData::ConfigSetting(
            ConfigSettingEntry::ContractCostParamsMemoryBytes(params),
        ) = &mem_entry.data
        {
            params.0.to_vec()
        } else {
            return Err(LedgerError::Internal(
                "Unexpected entry type for memory cost params".to_string(),
            ));
        };

        mem_params.resize(NEW_SIZE, make_entry(0, 0));

        // New BLS12-381 entries (indices 45..=69):
        mem_params[45] = make_entry(0, 0);                 // Bls12381EncodeFp
        mem_params[46] = make_entry(0, 0);                 // Bls12381DecodeFp
        mem_params[47] = make_entry(0, 0);                 // Bls12381G1CheckPointOnCurve
        mem_params[48] = make_entry(0, 0);                 // Bls12381G1CheckPointInSubgroup
        mem_params[49] = make_entry(0, 0);                 // Bls12381G2CheckPointOnCurve
        mem_params[50] = make_entry(0, 0);                 // Bls12381G2CheckPointInSubgroup
        mem_params[51] = make_entry(0, 0);                 // Bls12381G1ProjectiveToAffine
        mem_params[52] = make_entry(0, 0);                 // Bls12381G2ProjectiveToAffine
        mem_params[53] = make_entry(0, 0);                 // Bls12381G1Add
        mem_params[54] = make_entry(0, 0);                 // Bls12381G1Mul
        mem_params[55] = make_entry(109494, 354667);       // Bls12381G1Msm
        mem_params[56] = make_entry(5552, 0);              // Bls12381MapFpToG1
        mem_params[57] = make_entry(9424, 0);              // Bls12381HashToG1
        mem_params[58] = make_entry(0, 0);                 // Bls12381G2Add
        mem_params[59] = make_entry(0, 0);                 // Bls12381G2Mul
        mem_params[60] = make_entry(219654, 354667);       // Bls12381G2Msm
        mem_params[61] = make_entry(3344, 0);              // Bls12381MapFp2ToG2
        mem_params[62] = make_entry(6816, 0);              // Bls12381HashToG2
        mem_params[63] = make_entry(2204, 9340474);        // Bls12381Pairing
        mem_params[64] = make_entry(0, 0);                 // Bls12381FrFromU256
        mem_params[65] = make_entry(248, 0);               // Bls12381FrToU256
        mem_params[66] = make_entry(0, 0);                 // Bls12381FrAddSub
        mem_params[67] = make_entry(0, 0);                 // Bls12381FrMul
        mem_params[68] = make_entry(0, 128);               // Bls12381FrPow
        mem_params[69] = make_entry(0, 0);                 // Bls12381FrInv

        let new_mem_entry = LedgerEntry {
            last_modified_ledger_seq: self.close_data.ledger_seq,
            data: LedgerEntryData::ConfigSetting(
                ConfigSettingEntry::ContractCostParamsMemoryBytes(ContractCostParams(
                    mem_params.try_into().map_err(|_| {
                        LedgerError::Internal("Failed to convert V22 memory cost params".to_string())
                    })?,
                )),
            ),
            ext: LedgerEntryExt::V0,
        };
        self.delta.record_update(mem_entry, new_mem_entry)?;

        tracing::info!(
            ledger_seq = self.close_data.ledger_seq,
            new_size = NEW_SIZE,
            "Applied createCostTypesForV22: resized cost params with BLS12-381 entries"
        );

        Ok(())
    }

    /// Apply version upgrade side effects for protocol 23.
    ///
    /// Parity: NetworkConfig.cpp:1459-1484 `createAndUpdateLedgerEntriesForV23`
    /// Creates 3 new CONFIG_SETTING entries (parallel compute, SCP timing,
    /// ledger cost extension) and updates rent cost parameters.
    fn create_and_update_ledger_entries_for_v23(&mut self) -> Result<()> {
        use stellar_xdr::curr::{
            ConfigSettingContractLedgerCostExtV0, ConfigSettingContractParallelComputeV0,
            ConfigSettingScpTiming,
        };

        let ledger_seq = self.close_data.ledger_seq;

        // 1. CONFIG_SETTING_CONTRACT_PARALLEL_COMPUTE_V0
        // Parity: NetworkConfig.cpp:1069-1076 initialParallelComputeEntry
        self.delta.record_create(LedgerEntry {
            last_modified_ledger_seq: ledger_seq,
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::ContractParallelComputeV0(
                ConfigSettingContractParallelComputeV0 {
                    ledger_max_dependent_tx_clusters: 1, // LEDGER_MAX_DEPENDENT_TX_CLUSTERS
                },
            )),
            ext: LedgerEntryExt::V0,
        })?;

        // 2. CONFIG_SETTING_SCP_TIMING
        // Parity: NetworkConfig.cpp:1092-1108 initialScpTimingEntry
        self.delta.record_create(LedgerEntry {
            last_modified_ledger_seq: ledger_seq,
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::ScpTiming(
                ConfigSettingScpTiming {
                    ledger_target_close_time_milliseconds: 5000,
                    nomination_timeout_initial_milliseconds: 1000,
                    nomination_timeout_increment_milliseconds: 1000,
                    ballot_timeout_initial_milliseconds: 1000,
                    ballot_timeout_increment_milliseconds: 1000,
                },
            )),
            ext: LedgerEntryExt::V0,
        })?;

        // 3. CONFIG_SETTING_CONTRACT_LEDGER_COST_EXT_V0
        // Parity: NetworkConfig.cpp:1078-1090 initialLedgerCostExtEntry
        // Reads txMaxDiskReadEntries from the existing V0 cost setting.
        let cost_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractLedgerCostV0,
        });
        let cost_entry = self.load_entry(&cost_key)?.ok_or_else(|| {
            LedgerError::Internal(
                "CONFIG_SETTING_CONTRACT_LEDGER_COST_V0 not found (required for V23 upgrade)"
                    .to_string(),
            )
        })?;
        let tx_max_disk_read_entries = if let LedgerEntryData::ConfigSetting(
            ConfigSettingEntry::ContractLedgerCostV0(ref settings),
        ) = cost_entry.data
        {
            settings.tx_max_disk_read_entries
        } else {
            return Err(LedgerError::Internal(
                "Unexpected entry type for ContractLedgerCost".to_string(),
            ));
        };

        self.delta.record_create(LedgerEntry {
            last_modified_ledger_seq: ledger_seq,
            data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::ContractLedgerCostExtV0(
                ConfigSettingContractLedgerCostExtV0 {
                    tx_max_footprint_entries: tx_max_disk_read_entries,
                    fee_write1_kb: 3_500, // FEE_LEDGER_WRITE_1KB
                },
            )),
            ext: LedgerEntryExt::V0,
        })?;

        // 4. Update rent cost parameters
        // Parity: NetworkConfig.cpp:1142-1171 updateRentCostParamsForV23
        self.update_rent_cost_params_for_v23()?;

        tracing::info!(
            ledger_seq = self.close_data.ledger_seq,
            "Applied createAndUpdateLedgerEntriesForV23: 3 new entries + rent param update"
        );

        Ok(())
    }

    /// Update rent cost parameters for the V23 protocol upgrade.
    ///
    /// Parity: NetworkConfig.cpp:1142-1171 `updateRentCostParamsForV23`
    /// Updates ContractLedgerCost and StateArchival settings with new rent parameters.
    fn update_rent_cost_params_for_v23(&mut self) -> Result<()> {
        let ledger_seq = self.close_data.ledger_seq;

        // Update CONFIG_SETTING_CONTRACT_LEDGER_COST_V0
        let cost_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractLedgerCostV0,
        });
        let cost_entry = self.load_entry(&cost_key)?.ok_or_else(|| {
            LedgerError::Internal("ContractLedgerCostV0 entry not found".to_string())
        })?;

        if let LedgerEntryData::ConfigSetting(ConfigSettingEntry::ContractLedgerCostV0(
            ref settings,
        )) = cost_entry.data
        {
            let mut new_settings = settings.clone();
            // Protcol23UpgradedConfig values (note: typo matches stellar-core)
            new_settings.soroban_state_target_size_bytes = 3_000_000_000; // 3 GB
            new_settings.rent_fee1_kb_soroban_state_size_low = -17_000;
            new_settings.rent_fee1_kb_soroban_state_size_high = 10_000;

            let new_entry = LedgerEntry {
                last_modified_ledger_seq: ledger_seq,
                data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::ContractLedgerCostV0(
                    new_settings,
                )),
                ext: LedgerEntryExt::V0,
            };
            self.delta.record_update(cost_entry, new_entry)?;
        } else {
            return Err(LedgerError::Internal(
                "Unexpected entry type for ContractLedgerCostV0".to_string(),
            ));
        }

        // Update CONFIG_SETTING_STATE_ARCHIVAL
        let archival_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::StateArchival,
        });
        let archival_entry = self.load_entry(&archival_key)?.ok_or_else(|| {
            LedgerError::Internal("StateArchival entry not found".to_string())
        })?;

        if let LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(ref settings)) =
            archival_entry.data
        {
            let mut new_settings = settings.clone();
            new_settings.persistent_rent_rate_denominator = 1_215;
            new_settings.temp_rent_rate_denominator = 2_430;

            let new_entry = LedgerEntry {
                last_modified_ledger_seq: ledger_seq,
                data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(
                    new_settings,
                )),
                ext: LedgerEntryExt::V0,
            };
            self.delta.record_update(archival_entry, new_entry)?;
        } else {
            return Err(LedgerError::Internal(
                "Unexpected entry type for StateArchival".to_string(),
            ));
        }

        Ok(())
    }

    /// Apply version upgrade side effects for protocol 25.
    ///
    /// Parity: NetworkConfig.cpp:1450-1457 `createCostTypesForV25`
    /// Resizes the CPU and memory cost param entries to include BN254 curve
    /// cost types and populates their values.
    fn create_cost_types_for_v25(&mut self) -> Result<()> {
        use stellar_xdr::curr::{ContractCostParamEntry, ContractCostParams, ExtensionPoint};

        // BN254 cost type indices (from ContractCostType enum)
        const BN254_ENCODE_FP: usize = 70;
        const BN254_DECODE_FP: usize = 71;
        const BN254_G1_CHECK_POINT_ON_CURVE: usize = 72;
        const BN254_G2_CHECK_POINT_ON_CURVE: usize = 73;
        const BN254_G2_CHECK_POINT_IN_SUBGROUP: usize = 74;
        const BN254_G1_PROJECTIVE_TO_AFFINE: usize = 75;
        const BN254_G1_ADD: usize = 76;
        const BN254_G1_MUL: usize = 77;
        const BN254_PAIRING: usize = 78;
        const BN254_FR_FROM_U256: usize = 79;
        const BN254_FR_TO_U256: usize = 80;
        const BN254_FR_ADD_SUB: usize = 81;
        const BN254_FR_MUL: usize = 82;
        const BN254_FR_POW: usize = 83;
        const BN254_FR_INV: usize = 84;
        const NEW_SIZE: usize = BN254_FR_INV + 1; // 85

        let make_entry = |const_term: i64, linear_term: i64| ContractCostParamEntry {
            ext: ExtensionPoint::V0,
            const_term,
            linear_term,
        };

        // --- Update CPU cost params ---
        let cpu_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractCostParamsCpuInstructions,
        });
        let cpu_entry = self.load_entry(&cpu_key)?.ok_or_else(|| {
            LedgerError::Internal("ContractCostParamsCpuInstructions entry not found".to_string())
        })?;
        let mut cpu_params = if let LedgerEntryData::ConfigSetting(
            ConfigSettingEntry::ContractCostParamsCpuInstructions(params),
        ) = &cpu_entry.data
        {
            params.0.to_vec()
        } else {
            return Err(LedgerError::Internal(
                "Unexpected entry type for CPU cost params".to_string(),
            ));
        };

        // Resize to fit BN254 types
        cpu_params.resize(NEW_SIZE, make_entry(0, 0));

        // Set BN254 CPU cost values (from NetworkConfig.cpp:556-629)
        cpu_params[BN254_ENCODE_FP] = make_entry(344, 0);
        cpu_params[BN254_DECODE_FP] = make_entry(476, 0);
        cpu_params[BN254_G1_CHECK_POINT_ON_CURVE] = make_entry(904, 0);
        cpu_params[BN254_G2_CHECK_POINT_ON_CURVE] = make_entry(2811, 0);
        cpu_params[BN254_G2_CHECK_POINT_IN_SUBGROUP] = make_entry(2937755, 0);
        cpu_params[BN254_G1_PROJECTIVE_TO_AFFINE] = make_entry(61, 0);
        cpu_params[BN254_G1_ADD] = make_entry(3623, 0);
        cpu_params[BN254_G1_MUL] = make_entry(1150435, 0);
        cpu_params[BN254_PAIRING] = make_entry(5263916, 392472814);
        cpu_params[BN254_FR_FROM_U256] = make_entry(2052, 0);
        cpu_params[BN254_FR_TO_U256] = make_entry(1133, 0);
        cpu_params[BN254_FR_ADD_SUB] = make_entry(74, 0);
        cpu_params[BN254_FR_MUL] = make_entry(332, 0);
        cpu_params[BN254_FR_POW] = make_entry(755, 68930);
        cpu_params[BN254_FR_INV] = make_entry(33151, 0);

        let new_cpu_entry = LedgerEntry {
            last_modified_ledger_seq: self.close_data.ledger_seq,
            data: LedgerEntryData::ConfigSetting(
                ConfigSettingEntry::ContractCostParamsCpuInstructions(ContractCostParams(
                    cpu_params.try_into().map_err(|_| {
                        LedgerError::Internal("Failed to convert CPU cost params".to_string())
                    })?,
                )),
            ),
            ext: LedgerEntryExt::V0,
        };
        self.delta.record_update(cpu_entry, new_cpu_entry)?;

        // --- Update memory cost params ---
        let mem_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractCostParamsMemoryBytes,
        });
        let mem_entry = self.load_entry(&mem_key)?.ok_or_else(|| {
            LedgerError::Internal("ContractCostParamsMemoryBytes entry not found".to_string())
        })?;
        let mut mem_params = if let LedgerEntryData::ConfigSetting(
            ConfigSettingEntry::ContractCostParamsMemoryBytes(params),
        ) = &mem_entry.data
        {
            params.0.to_vec()
        } else {
            return Err(LedgerError::Internal(
                "Unexpected entry type for memory cost params".to_string(),
            ));
        };

        // Resize to fit BN254 types
        mem_params.resize(NEW_SIZE, make_entry(0, 0));

        // Set BN254 memory cost values (from NetworkConfig.cpp:993-1067)
        // Most are 0,0 except Bn254Pairing and Bn254FrToU256
        mem_params[BN254_ENCODE_FP] = make_entry(0, 0);
        mem_params[BN254_DECODE_FP] = make_entry(0, 0);
        mem_params[BN254_G1_CHECK_POINT_ON_CURVE] = make_entry(0, 0);
        mem_params[BN254_G2_CHECK_POINT_ON_CURVE] = make_entry(0, 0);
        mem_params[BN254_G2_CHECK_POINT_IN_SUBGROUP] = make_entry(0, 0);
        mem_params[BN254_G1_PROJECTIVE_TO_AFFINE] = make_entry(0, 0);
        mem_params[BN254_G1_ADD] = make_entry(0, 0);
        mem_params[BN254_G1_MUL] = make_entry(0, 0);
        mem_params[BN254_PAIRING] = make_entry(1821, 6232546);
        mem_params[BN254_FR_FROM_U256] = make_entry(0, 0);
        mem_params[BN254_FR_TO_U256] = make_entry(312, 0);
        mem_params[BN254_FR_ADD_SUB] = make_entry(0, 0);
        mem_params[BN254_FR_MUL] = make_entry(0, 0);
        mem_params[BN254_FR_POW] = make_entry(0, 0);
        mem_params[BN254_FR_INV] = make_entry(0, 0);

        let new_mem_entry = LedgerEntry {
            last_modified_ledger_seq: self.close_data.ledger_seq,
            data: LedgerEntryData::ConfigSetting(
                ConfigSettingEntry::ContractCostParamsMemoryBytes(ContractCostParams(
                    mem_params.try_into().map_err(|_| {
                        LedgerError::Internal("Failed to convert memory cost params".to_string())
                    })?,
                )),
            ),
            ext: LedgerEntryExt::V0,
        };
        self.delta.record_update(mem_entry, new_mem_entry)?;

        tracing::info!(
            ledger_seq = self.close_data.ledger_seq,
            new_size = NEW_SIZE,
            "Applied createCostTypesForV25: resized cost params with BN254 entries"
        );

        Ok(())
    }

    /// Load Soroban rent config from the delta (for upgraded values) falling back to snapshot.
    ///
    /// This is used during config upgrades where the new cost params are in the delta
    /// but haven't been applied to the bucket list yet.
    /// Parity: stellar-core loads from LedgerTxn which reflects the just-applied upgrades.
    fn load_rent_config_from_delta_or_snapshot(
        &self,
    ) -> Option<crate::soroban_state::SorobanRentConfig> {
        let cpu_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractCostParamsCpuInstructions,
        });
        let cpu_params = self.load_entry(&cpu_key).ok()?.and_then(|e| {
            if let LedgerEntryData::ConfigSetting(
                ConfigSettingEntry::ContractCostParamsCpuInstructions(params),
            ) = e.data
            {
                Some(params)
            } else {
                None
            }
        })?;

        let mem_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractCostParamsMemoryBytes,
        });
        let mem_params = self.load_entry(&mem_key).ok()?.and_then(|e| {
            if let LedgerEntryData::ConfigSetting(
                ConfigSettingEntry::ContractCostParamsMemoryBytes(params),
            ) = e.data
            {
                Some(params)
            } else {
                None
            }
        })?;

        let compute_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::ContractComputeV0,
        });
        let (tx_max_instructions, tx_max_memory_bytes) =
            self.load_entry(&compute_key).ok()?.and_then(|e| {
                if let LedgerEntryData::ConfigSetting(ConfigSettingEntry::ContractComputeV0(
                    compute,
                )) = e.data
                {
                    Some((
                        compute.tx_max_instructions as u64,
                        compute.tx_memory_limit as u64,
                    ))
                } else {
                    None
                }
            })?;

        Some(crate::soroban_state::SorobanRentConfig {
            cpu_cost_params: cpu_params,
            mem_cost_params: mem_params,
            tx_max_instructions,
            tx_max_memory_bytes,
        })
    }

    /// Apply transactions from the transaction set.
    ///
    /// This executes all transactions in order, recording state changes
    /// to the delta and collecting results.
    ///
    /// The executor is persisted across ledger closes to avoid reloading ~911K
    /// offers from the in-memory offer store on every ledger (~2.7s on mainnet).
    /// On the first call after initialization (or reset), a new executor is
    /// created and offers are loaded once. On subsequent calls, the executor is
    /// advanced via `advance_to_ledger_preserving_offers` which clears non-offer
    /// cached entries while keeping the offer index intact.
    fn apply_transactions(&mut self) -> Result<Vec<TransactionExecutionResult>> {
        use henyey_common::protocol::PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION;

        let prepare_start = std::time::Instant::now();
        let tx_set_hash = self.close_data.tx_set_hash();
        let prepared = if self.close_data.presorted {
            // Take ownership of tx_set to avoid cloning 50K envelopes.
            // Replace with empty classic set; the original is only needed for meta later
            // (which is skipped in simulation mode).
            let tx_set = std::mem::replace(
                &mut self.close_data.tx_set,
                TransactionSetVariant::Classic(stellar_xdr::curr::TransactionSet {
                    previous_ledger_hash: Hash([0; 32]),
                    txs: Default::default(),
                }),
            );
            tx_set.prepare_presorted(tx_set_hash)
        } else {
            self.close_data.tx_set.prepare_with_hash(tx_set_hash)
        };
        let prepare_us = prepare_start.elapsed().as_micros() as u64;

        if prepared.all_txs.is_empty() {
            self.tx_results.clear();
            return Ok(vec![]);
        }

        // Load SorobanConfig from ledger ConfigSettingEntry for accurate Soroban execution
        let config_load_start = std::time::Instant::now();
        let soroban_config =
            crate::execution::load_soroban_config(&self.snapshot, self.prev_header.ledger_version);
        // Cache fee_write_1kb for LedgerCloseMetaExtV1 (set during commit phase).
        // This is stellar-core's feeRent1KB() / sorobanFeeWrite1KB.
        self.soroban_fee_write_1kb = soroban_config.rent_fee_config.fee_per_write_1kb;
        // Use transaction set hash as base PRNG seed for Soroban execution
        let soroban_base_prng_seed = prepared.hash;
        let classic_events = ClassicEventConfig {
            emit_classic_events: self.manager.config.emit_classic_events,
            backfill_stellar_asset_events: self.manager.config.backfill_stellar_asset_events,
        };

        // Get reference to the module cache for Soroban contract execution.
        // The cache is pre-initialized from bucket list CONTRACT_CODE entries.
        let module_cache_guard = self.manager.module_cache.read();
        let module_cache = module_cache_guard.as_ref();

        // Get the hot archive for Protocol 23+ entry restoration.
        // We pass the Arc directly - the execution layer will check if it contains Some.
        let hot_archive = Some(self.manager.hot_archive_bucket_list.clone());

        // Check if we have a structured Soroban phase (V1 TransactionPhase).
        // In stellar-core, the Soroban phase ALWAYS goes through
        // applyParallelPhase() when it has V1 structure (isParallel()=true),
        // regardless of cluster count. Refunds are applied in
        // processPostTxSetApply() which only handles parallel phases.
        // The sequential path (applySequentialPhase) does NOT apply refunds
        // for P23+. So we must always use the parallel path when a Soroban
        // phase structure exists, even for single-stage single-cluster sets.
        let has_parallel = prepared.soroban_phase.is_some();

        let config_load_us = config_load_start.elapsed().as_micros() as u64;

        // Take the persistent executor from the manager, or create a new one.
        // The executor's offer cache is preserved across ledger closes to avoid
        // reloading ~911K offers each time.
        let executor_setup_start = std::time::Instant::now();
        let mut executor = self.manager.executor.lock().take();
        let is_new_executor = executor.is_none();
        let id_pool = self.snapshot.header().id_pool;

        let executor_ref = executor.get_or_insert_with(|| {
            let ctx = LedgerContext::new(
                self.close_data.ledger_seq,
                self.close_data.close_time,
                self.prev_header.base_fee,
                self.prev_header.base_reserve,
                self.prev_header.ledger_version,
                self.manager.network_id,
            );
            TransactionExecutor::new(
                &ctx,
                id_pool,
                soroban_config.clone(),
                classic_events,
            )
        });

        if is_new_executor {
            // First ledger after init/reset: set up shared offer store
            executor_ref.set_offer_store(self.manager.offer_store.clone());
            if let Some(cache) = module_cache {
                executor_ref.set_module_cache(cache.clone());
            }
            if let Some(ref ha) = hot_archive {
                executor_ref.set_hot_archive(ha.clone());
            }
        } else {
            // Subsequent ledgers: advance the executor, preserving offers
            executor_ref.advance_to_ledger_preserving_offers(
                self.close_data.ledger_seq,
                self.close_data.close_time,
                self.prev_header.base_reserve,
                self.prev_header.ledger_version,
                id_pool,
                soroban_config.clone(),
            );
            // Update module cache and hot archive references (they may have changed)
            if let Some(cache) = module_cache {
                executor_ref.set_module_cache(cache.clone());
            }
            if let Some(ref ha) = hot_archive {
                executor_ref.set_hot_archive(ha.clone());
            }
        }

        // Configure meta extension flags from LedgerManagerConfig.
        executor_ref.set_meta_flags(
            self.manager.config.emit_soroban_tx_meta_ext_v1,
            self.manager.config.enable_soroban_diagnostic_events,
        );
        let executor_setup_us = executor_setup_start.elapsed().as_micros() as u64;

        let mut fee_pre_deduct_us: u64 = 0;
        let mut tx_set_result =
            if has_parallel
                && protocol_version_starts_from(
                    self.prev_header.ledger_version,
                    PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION,
                )
            {
                let phase = prepared.soroban_phase.as_ref().unwrap();
                let classic_txs = &prepared.classic_txs;

                // Pre-deduct ALL fees (classic + Soroban) in a single pass before
                // any transaction body executes. This matches stellar-core's
                // processFeesSeqNums() which processes all phases' fees in order
                // before any transaction applies.
                let fee_pre_deduct_start = std::time::Instant::now();
                let (classic_pre_charged, soroban_pre_charged, total_fee_pool) =
                    pre_deduct_all_fees_on_delta(
                        classic_txs,
                        phase,
                        self.prev_header.base_fee,
                        self.manager.network_id,
                        self.close_data.ledger_seq,
                        &mut self.delta,
                        &self.snapshot,
                    )?;
                self.delta.record_fee_pool_delta(total_fee_pool);

                // Pre-load fee-deducted account entries from the delta into the
                // classic executor so classic TXs see ALL fee deductions (including
                // Soroban fees on shared accounts).
                for entry in self.delta.current_entries() {
                    if matches!(entry.data, stellar_xdr::curr::LedgerEntryData::Account(_)) {
                        executor_ref.state_mut().load_entry(entry);
                    }
                }

                fee_pre_deduct_us = fee_pre_deduct_start.elapsed().as_micros() as u64;

                // Execute classic phase (fees already deducted on delta).
                let classic_start = std::time::Instant::now();
                let mut classic_result = if classic_txs.is_empty() {
                    TxSetResult {
                        results: Vec::new(),
                        tx_results: Vec::new(),
                        tx_result_metas: Vec::new(),
                        id_pool: self.snapshot.header().id_pool,
                        hot_archive_restored_keys: Vec::new(),
                    }
                } else {
                    run_transactions_on_executor(
                        executor_ref,
                        &self.snapshot,
                        classic_txs,
                        self.prev_header.base_fee,
                        soroban_base_prng_seed.0,
                        false,
                        &mut self.delta,
                        Some(&classic_pre_charged),
                    )?
                };
                self.timing_classic_exec_us = classic_start.elapsed().as_micros() as u64;

                // Execute Soroban parallel phase (fees already deducted on delta).
                let soroban_start = std::time::Instant::now();
                let ledger_context = LedgerContext::new(
                    self.close_data.ledger_seq,
                    self.close_data.close_time,
                    self.prev_header.base_fee,
                    self.prev_header.base_reserve,
                    self.prev_header.ledger_version,
                    self.manager.network_id,
                );
                let soroban_result = execute_soroban_parallel_phase(
                    &self.snapshot,
                    phase,
                    classic_txs.len(),
                    &ledger_context,
                    &mut self.delta,
                    SorobanContext {
                        config: soroban_config,
                        base_prng_seed: soroban_base_prng_seed.0,
                        classic_events,
                        module_cache,
                        hot_archive,
                        runtime_handle: self.runtime_handle.clone(),
                        soroban_state: Some(self.manager.soroban_state.clone()),
                        offer_store: Some(self.manager.offer_store.clone()),
                        emit_soroban_tx_meta_ext_v1: self.manager.config.emit_soroban_tx_meta_ext_v1,
                        enable_soroban_diagnostic_events: self.manager.config.enable_soroban_diagnostic_events,
                    },
                    Some(soroban_pre_charged),
                )?;
                self.timing_soroban_exec_us = soroban_start.elapsed().as_micros() as u64;

                // Combine results: classic first, then Soroban.
                classic_result.results.extend(soroban_result.results);
                classic_result.tx_results.extend(soroban_result.tx_results);
                classic_result.tx_result_metas.extend(soroban_result.tx_result_metas);
                classic_result.id_pool = classic_result.id_pool.max(soroban_result.id_pool);
                classic_result.hot_archive_restored_keys.extend(soroban_result.hot_archive_restored_keys);

                tracing::debug!(
                    ledger_seq = self.close_data.ledger_seq,
                    classic_tx_count = classic_txs.len(),
                    soroban_stages = phase.stages.len(),
                    soroban_clusters = phase.stages.iter().map(|s| s.len()).sum::<usize>(),
                    "Executed parallel Soroban phase"
                );

                classic_result
            } else {
                // Sequential path: run all transactions on the persistent executor.
                run_transactions_on_executor(
                    executor_ref,
                    &self.snapshot,
                    &prepared.all_txs,
                    self.prev_header.base_fee,
                    soroban_base_prng_seed.0,
                    true,
                    &mut self.delta,
                    None,
                )?
            };

        // Store the executor back for reuse on the next ledger close
        *self.manager.executor.lock() = executor;

        let post_exec_start = std::time::Instant::now();
        // Prepend fee events for classic event emission.
        if classic_events.events_enabled(self.prev_header.ledger_version) {
            for (idx, meta) in tx_set_result.tx_result_metas.iter_mut().enumerate() {
                if idx >= tx_set_result.tx_results.len() || idx >= prepared.tx_meta.len() {
                    break;
                }
                let fee_charged = tx_set_result.tx_results[idx].result.fee_charged;
                let fee_source = &prepared.tx_meta[idx].fee_source;
                prepend_fee_event(
                    &mut meta.tx_apply_processing,
                    fee_source,
                    fee_charged,
                    self.prev_header.ledger_version,
                    &self.manager.network_id,
                    classic_events,
                );
            }
        }

        self.id_pool = tx_set_result.id_pool;
        self.tx_results = tx_set_result.tx_results;
        self.tx_result_metas = tx_set_result.tx_result_metas;
        self.hot_archive_restored_keys = tx_set_result.hot_archive_restored_keys;

        // Update stats
        let tx_count = tx_set_result.results.len();
        let success_count = tx_set_result.results.iter().filter(|r| r.success).count();
        let op_count: usize = tx_set_result.results.iter().map(|r| r.operation_results.len()).sum();
        let fees_collected: i64 = tx_set_result.results.iter().map(|r| r.fee_charged).sum();

        self.stats
            .record_transactions(tx_count, success_count, op_count);
        self.stats.record_fees(fees_collected);

        // Collect per-transaction perf data and aggregate sub-phase timings
        let mut agg_op_type_timings: HashMap<henyey_tx::OperationType, (u64, u32)> = HashMap::new();
        let (mut agg_validation_us, mut agg_fee_seq_us, mut agg_footprint_us, mut agg_ops_us, mut agg_meta_build_us) = (0u64, 0u64, 0u64, 0u64, 0u64);
        let (mut agg_val_account_load_us, mut agg_val_tx_hash_us, mut agg_val_ed25519_us, mut agg_val_other_us) = (0u64, 0u64, 0u64, 0u64);
        let (mut agg_fee_deduct_us, mut agg_op_sig_check_us, mut agg_signer_removal_us, mut agg_seq_bump_us) = (0u64, 0u64, 0u64, 0u64);
        for (i, result) in tx_set_result.results.iter().enumerate() {
            let hash_hex = if i < self.tx_results.len() {
                Hash256::from_bytes(self.tx_results[i].transaction_hash.0).to_hex()[..16].to_string()
            } else {
                String::new()
            };
            let is_soroban = prepared.tx_meta.get(i).map_or(false, |m| m.is_soroban);
            self.tx_perf.push(crate::close::TxPerf {
                index: i,
                hash_hex,
                success: result.success,
                op_count: result.operation_results.len(),
                exec_us: result.exec_time_us,
                is_soroban,
            });
            // Aggregate per-TX sub-phase timings
            agg_validation_us += result.validation_us;
            agg_fee_seq_us += result.fee_seq_us;
            agg_footprint_us += result.footprint_us;
            agg_ops_us += result.ops_us;
            agg_meta_build_us += result.meta_build_us;
            agg_val_account_load_us += result.val_account_load_us;
            agg_val_tx_hash_us += result.val_tx_hash_us;
            agg_val_ed25519_us += result.val_ed25519_us;
            agg_val_other_us += result.val_other_us;
            agg_fee_deduct_us += result.fee_deduct_us;
            agg_op_sig_check_us += result.op_sig_check_us;
            agg_signer_removal_us += result.signer_removal_us;
            agg_seq_bump_us += result.seq_bump_us;
            // Aggregate per-op-type timings across all TXs
            for (op_type, (us, count)) in &result.op_type_timings {
                let entry = agg_op_type_timings.entry(*op_type).or_insert((0, 0));
                entry.0 += us;
                entry.1 += count;
            }
        }
        let post_exec_us = post_exec_start.elapsed().as_micros() as u64;

        // Build per-op-type timing summary sorted by time desc
        let mut op_timing_vec: Vec<_> = agg_op_type_timings.iter().collect();
        op_timing_vec.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
        let op_timing_str: String = op_timing_vec
            .iter()
            .map(|(op, (us, count))| format!("{:?}:{}us×{}", op, us, count))
            .collect::<Vec<_>>()
            .join(",");

        // Emit comprehensive profiling line at debug level
        debug!(
            ledger_seq = self.close_data.ledger_seq,
            tx_count = tx_count,
            op_count = op_count,
            prepare_us,
            config_load_us,
            executor_setup_us,
            fee_pre_deduct_us,
            classic_exec_us = self.timing_classic_exec_us,
            soroban_exec_us = self.timing_soroban_exec_us,
            post_exec_us,
            agg_validation_us,
            agg_val_account_load_us,
            agg_val_tx_hash_us,
            agg_val_ed25519_us,
            agg_val_other_us,
            agg_fee_seq_us,
            agg_fee_deduct_us,
            agg_op_sig_check_us,
            agg_signer_removal_us,
            agg_seq_bump_us,
            agg_footprint_us,
            agg_ops_us,
            agg_meta_build_us,
            op_timings = %op_timing_str,
            "PROFILE apply_txs"
        );

        // Store sub-phase timings for LedgerClosePerf.
        self.timing_prepare_us = prepare_us;
        self.timing_config_load_us = config_load_us;
        self.timing_executor_setup_us = executor_setup_us;
        self.timing_fee_pre_deduct_us = fee_pre_deduct_us;
        self.timing_post_exec_us = post_exec_us;

        Ok(tx_set_result.results)
    }

    /// Apply protocol version upgrades and config upgrades to the delta.
    ///
    /// Handles V25 cost type creation, config setting upgrades, and
    /// Soroban state size window recomputation when memory cost params change.
    /// Returns `(config_state_archival_changed, config_memory_cost_params_changed)`.
    fn apply_upgrades_to_delta(
        &mut self,
        prev_version: u32,
        protocol_version: u32,
    ) -> Result<(bool, bool, Vec<UpgradeEntryMeta>)> {
        use stellar_xdr::curr::{LedgerEntryChange, LedgerEntryChanges, LedgerUpgrade, Limits, WriteXdr};

        // Parity: Upgrades.cpp:1229-1242 applyVersionUpgrade
        // Version upgrades may create/modify config setting entries in the
        // ledger (e.g. new cost types for V25, state size window for V23+).
        // These must be applied to the delta before bucket list extraction.
        let mut version_upgrade_memory_cost_changed = false;

        // Capture changes from version upgrade side effects (cost types for V25).
        // We record the delta state before and after to extract changes.
        let version_changes = if prev_version != protocol_version {
            let delta_before = self.delta.num_changes();
            // Parity: Upgrades.cpp:1189-1212
            // needUpgradeToVersion(V_20, prev, new) → createLedgerEntriesForV20
            if needs_upgrade_to_version(ProtocolVersion::V20, prev_version, protocol_version) {
                self.create_ledger_entries_for_v20()?;
                version_upgrade_memory_cost_changed = true;
            }

            // Parity: Upgrades.cpp:1213-1217
            // needUpgradeToVersion(V_21, prev, new) → createCostTypesForV21
            if needs_upgrade_to_version(ProtocolVersion::V21, prev_version, protocol_version) {
                self.create_cost_types_for_v21()?;
                version_upgrade_memory_cost_changed = true;
            }

            // Parity: Upgrades.cpp:1219-1223
            // needUpgradeToVersion(V_22, prev, new) → createCostTypesForV22
            if needs_upgrade_to_version(ProtocolVersion::V22, prev_version, protocol_version) {
                self.create_cost_types_for_v22()?;
                version_upgrade_memory_cost_changed = true;
            }

            // Parity: Upgrades.cpp:1225-1229
            // needUpgradeToVersion(V_23, prev, new) → createAndUpdateLedgerEntriesForV23
            if needs_upgrade_to_version(ProtocolVersion::V23, prev_version, protocol_version) {
                self.create_and_update_ledger_entries_for_v23()?;
                version_upgrade_memory_cost_changed = true;
            }

            // Parity: Upgrades.cpp:1229-1233
            // needUpgradeToVersion(V_25, prev, new) → createCostTypesForV25
            if needs_upgrade_to_version(ProtocolVersion::V25, prev_version, protocol_version) {
                self.create_cost_types_for_v25()?;
                version_upgrade_memory_cost_changed = true;
            }

            // Parity: Upgrades.cpp:1189-1193
            // needUpgradeToVersion(V_10, prev, new) → prepareLiabilities
            // In stellar-core, the version upgrade runs prepareLiabilities with
            // the OLD base_reserve (the header hasn't been updated for reserve
            // yet). If there's also a reserve increase, applyReserveUpgrade
            // will run prepareLiabilities a second time with the new reserve.
            //
            // NOTE: Henyey supports protocol 24+ only, so prev_version < 10
            // should never be true in production. This is included for
            // completeness.
            if needs_upgrade_to_version(ProtocolVersion::V10, prev_version, protocol_version) {
                crate::prepare_liabilities::prepare_liabilities(
                    &self.snapshot,
                    &mut self.delta,
                    protocol_version,
                    self.prev_header.base_reserve,
                    self.close_data.ledger_seq,
                )?;
            }

            // Parity: Upgrades.cpp:1244-1251
            // prevVersion==V_23 && newVersion==V_24 && gIsProductionNetwork
            // Correct for 3.1879035 XLM fee burn that occurred during protocol 23 on mainnet.
            if prev_version == 23
                && protocol_version == 24
                && self.manager.network_id().is_mainnet()
            {
                self.delta.record_fee_pool_delta(31_879_035);
                tracing::info!(
                    "Applied V24 mainnet fee pool correction: +31879035 stroops"
                );
            }
            // Extract changes made during version upgrade side effects
            let mut changes: Vec<LedgerEntryChange> = Vec::new();
            let delta_after = self.delta.num_changes();
            if delta_after > delta_before {
                for change in self.delta.changes().skip(delta_before) {
                    match change {
                        crate::delta::EntryChange::Created(entry) => {
                            changes.push(LedgerEntryChange::Created(entry.clone()));
                        }
                        crate::delta::EntryChange::Updated { previous, current } => {
                            changes.push(LedgerEntryChange::State(previous.clone()));
                            changes.push(LedgerEntryChange::Updated(current.as_ref().clone()));
                        }
                        crate::delta::EntryChange::Deleted { previous } => {
                            let key = henyey_common::entry_to_key(previous);
                            changes.push(LedgerEntryChange::State(previous.clone()));
                            changes.push(LedgerEntryChange::Removed(key));
                        }
                    }
                }
            }
            LedgerEntryChanges(changes.try_into().unwrap_or_default())
        } else {
            LedgerEntryChanges(VecM::default())
        };

        // Parity: Upgrades.cpp:1254-1267 applyReserveUpgrade
        // If the base reserve increased and protocol >= V10, run prepareLiabilities.
        // This handles the case where a reserve increase makes some offers
        // unsupportable due to the higher minimum balance.
        //
        // NOTE: The V10 version upgrade path above could theoretically also
        // trigger prepareLiabilities, but Henyey only supports protocol 24+,
        // so that path is never reached. If both were active, stellar-core
        // would run prepareLiabilities twice (once per upgrade). Our
        // snapshot-based architecture doesn't support that two-pass pattern
        // without a merged snapshot view, but since the V10 path is dead code,
        // this is not an issue in practice.
        let reserve_changes = if let Some(new_reserve) = self.upgrade_ctx.base_reserve_upgrade() {
            let did_reserve_increase = new_reserve > self.prev_header.base_reserve;
            if protocol_version_starts_from(protocol_version, ProtocolVersion::V10) && did_reserve_increase {
                let delta_before = self.delta.num_changes();
                crate::prepare_liabilities::prepare_liabilities(
                    &self.snapshot,
                    &mut self.delta,
                    protocol_version,
                    new_reserve,
                    self.close_data.ledger_seq,
                )?;
                // Extract changes for UpgradeEntryMeta
                let mut changes: Vec<LedgerEntryChange> = Vec::new();
                let delta_after = self.delta.num_changes();
                if delta_after > delta_before {
                    for change in self.delta.changes().skip(delta_before) {
                        match change {
                            crate::delta::EntryChange::Created(entry) => {
                                changes.push(LedgerEntryChange::Created(entry.clone()));
                            }
                            crate::delta::EntryChange::Updated { previous, current } => {
                                changes.push(LedgerEntryChange::State(previous.clone()));
                                changes.push(LedgerEntryChange::Updated(current.as_ref().clone()));
                            }
                            crate::delta::EntryChange::Deleted { previous } => {
                                let key = henyey_common::entry_to_key(previous);
                                changes.push(LedgerEntryChange::State(previous.clone()));
                                changes.push(LedgerEntryChange::Removed(key));
                            }
                        }
                    }
                }
                LedgerEntryChanges(changes.try_into().unwrap_or_default())
            } else {
                LedgerEntryChanges(VecM::default())
            }
        } else {
            LedgerEntryChanges(VecM::default())
        };

        // Apply config upgrades to the delta BEFORE extracting entries for the bucket list.
        // In stellar-core, config upgrades are applied to the LedgerTxn before
        // getAllEntries() and addBatch(), so the upgraded ConfigSetting entries are included
        // in the bucket list update. We must do the same here.
        let mut config_state_archival_changed = false;
        let mut config_memory_cost_params_changed = false;
        let mut per_config_changes: HashMap<Vec<u8>, LedgerEntryChanges> = HashMap::new();
        let delta_count_before_upgrades = self.delta.num_changes();
        if self.upgrade_ctx.has_config_upgrades() {
            let result = self
                .upgrade_ctx
                .apply_config_upgrades(&self.snapshot, &mut self.delta)?;
            config_state_archival_changed = result.state_archival_changed;
            config_memory_cost_params_changed = result.memory_cost_params_changed;
            per_config_changes = result.per_upgrade_changes;
            tracing::info!(
                ledger_seq = self.close_data.ledger_seq,
                delta_before = delta_count_before_upgrades,
                delta_after = self.delta.num_changes(),
                archival_changed = config_state_archival_changed,
                memory_cost_changed = config_memory_cost_params_changed,
                "Delta entry count after config upgrades"
            );
        }

        // Apply MaxSorobanTxSetSize upgrade to the delta (modifies CONFIG_SETTING entry).
        // Parity: Upgrades.cpp upgradeMaxSorobanTxSetSize()
        let max_soroban_changes = if self.upgrade_ctx.max_soroban_tx_set_size_upgrade().is_some() {
            self.upgrade_ctx.apply_max_soroban_tx_set_size(
                &self.snapshot,
                &mut self.delta,
                self.close_data.ledger_seq,
            )?
        } else {
            LedgerEntryChanges(VecM::default())
        };

        // Build UpgradeEntryMeta for each upgrade.
        // Parity: LedgerManagerImpl.cpp:1660-1673
        let mut upgrades_meta = Vec::new();
        for upgrade in std::mem::take(&mut self.close_data.upgrades) {
            let changes = match &upgrade {
                LedgerUpgrade::Version(_) => version_changes.clone(),
                LedgerUpgrade::Config(key) => {
                    let key_bytes = key.to_xdr(Limits::none()).unwrap_or_default();
                    per_config_changes.remove(&key_bytes).unwrap_or_else(|| {
                        LedgerEntryChanges(VecM::default())
                    })
                }
                LedgerUpgrade::MaxSorobanTxSetSize(_) => max_soroban_changes.clone(),
                LedgerUpgrade::BaseReserve(_) => reserve_changes.clone(),
                _ => LedgerEntryChanges(VecM::default()),
            };
            upgrades_meta.push(UpgradeEntryMeta {
                upgrade,
                changes,
            });
        }

        // Parity: Upgrades.cpp:1238-1242 and 1449-1453
        // handleUpgradeAffectingSorobanInMemoryStateSize is called:
        // 1. After version upgrade to V23+ (recompute with potentially new cost params)
        // 2. After config upgrade that changes ContractCostParamsMemoryBytes
        // It recomputes contract code sizes in-memory and overwrites all window entries.
        let version_upgrade_triggers_state_size =
            prev_version != protocol_version && protocol_version_starts_from(protocol_version, ProtocolVersion::V23);
        if (config_memory_cost_params_changed
            || version_upgrade_memory_cost_changed
            || version_upgrade_triggers_state_size)
            && protocol_version >= henyey_common::MIN_SOROBAN_PROTOCOL_VERSION
        {
            // Load rent config from delta (new upgraded values) falling back to snapshot.
            // stellar-core loads from LedgerTxn which reflects the just-applied upgrades.
            let rent_config = self.load_rent_config_from_delta_or_snapshot();

            // Recompute contract code sizes with new cost params
            {
                let mut soroban_state = self.manager.soroban_state.write();
                let code_size_before = soroban_state.contract_code_state_size();
                let data_size_before = soroban_state.contract_data_state_size();
                let code_count = soroban_state.contract_code_count();
                let data_count = soroban_state.contract_data_count();
                soroban_state.recompute_contract_code_sizes(protocol_version, rent_config.as_ref());
                tracing::info!(
                    ledger_seq = self.close_data.ledger_seq,
                    code_size_before = code_size_before,
                    code_size_after = soroban_state.contract_code_state_size(),
                    data_size = data_size_before,
                    code_count = code_count,
                    data_count = data_count,
                    total_size = soroban_state.total_size(),
                    has_rent_config = rent_config.is_some(),
                    "Recomputed contract code sizes"
                );
            }

            // Update all window entries with the new total size
            // Parity: NetworkConfig.cpp:2165 updateRecomputedSorobanStateSize
            if protocol_version_starts_from(
                protocol_version,
                ProtocolVersion::V23,
            ) {
                let new_size = self.manager.soroban_state.read().total_size();
                let window_key = stellar_xdr::curr::LedgerKey::ConfigSetting(
                    stellar_xdr::curr::LedgerKeyConfigSetting {
                        config_setting_id:
                            stellar_xdr::curr::ConfigSettingId::LiveSorobanStateSizeWindow,
                    },
                );

                // Read the window from the delta first (it may have been resized
                // by the config upgrade), falling back to the snapshot.
                // Parity: stellar-core reads from LedgerTxn which includes prior modifications.
                let (window_vec_base, previous_entry) = {
                    let delta_change = self.delta.get_change(&window_key);
                    if let Some(change) = delta_change {
                        if let Some(current) = change.current_entry() {
                            if let stellar_xdr::curr::LedgerEntryData::ConfigSetting(
                                stellar_xdr::curr::ConfigSettingEntry::LiveSorobanStateSizeWindow(
                                    w,
                                ),
                            ) = &current.data
                            {
                                // Use delta's current version (includes resize)
                                // For the "previous" in record_update, use the snapshot version
                                let snapshot_entry =
                                    self.snapshot.get_entry(&window_key).ok().flatten();
                                (
                                    Some(w.iter().copied().collect::<Vec<u64>>()),
                                    snapshot_entry,
                                )
                            } else {
                                (None, None)
                            }
                        } else {
                            (None, None)
                        }
                    } else if let Some(entry) = self.snapshot.get_entry(&window_key).ok().flatten()
                    {
                        if let stellar_xdr::curr::LedgerEntryData::ConfigSetting(
                            stellar_xdr::curr::ConfigSettingEntry::LiveSorobanStateSizeWindow(w),
                        ) = &entry.data
                        {
                            (Some(w.iter().copied().collect::<Vec<u64>>()), Some(entry))
                        } else {
                            (None, None)
                        }
                    } else {
                        (None, None)
                    }
                };

                if let (Some(mut window_vec), Some(prev)) = (window_vec_base, previous_entry) {
                    for size in &mut window_vec {
                        *size = new_size;
                    }
                    let new_window: stellar_xdr::curr::VecM<u64> =
                        window_vec.try_into().map_err(|_| {
                            LedgerError::Internal("Failed to convert window vec".to_string())
                        })?;
                    let new_window_entry = stellar_xdr::curr::LedgerEntry {
                        last_modified_ledger_seq: self.close_data.ledger_seq,
                        data: stellar_xdr::curr::LedgerEntryData::ConfigSetting(
                            stellar_xdr::curr::ConfigSettingEntry::LiveSorobanStateSizeWindow(
                                new_window,
                            ),
                        ),
                        ext: stellar_xdr::curr::LedgerEntryExt::V0,
                    };
                    self.delta.record_update(prev, new_window_entry)?;
                    tracing::info!(
                        ledger_seq = self.close_data.ledger_seq,
                        new_size = new_size,
                        delta_count = self.delta.num_changes(),
                        "Updated all state size window entries due to memory cost params upgrade"
                    );
                }
            }
        }

        Ok((config_state_archival_changed, config_memory_cost_params_changed, upgrades_meta))
    }

    /// Create the new ledger header and compute its hash.
    ///
    /// Applies upgrades to header fields, encodes raw upgrade types for
    /// correct header hash, and sets the id_pool.
    fn build_and_hash_header(
        &self,
        bucket_list_hash: Hash256,
        tx_result_hash: Hash256,
        config_state_archival_changed: bool,
        config_memory_cost_params_changed: bool,
    ) -> Result<(LedgerHeader, Hash256)> {
        // Log all inputs to create_next_header for debugging header mismatch
        let total_coins = self.prev_header.total_coins + self.delta.total_coins_delta();
        let fee_pool = self.prev_header.fee_pool + self.delta.fee_pool_delta();
        tracing::debug!(
            ledger_seq = self.close_data.ledger_seq,
            prev_header_hash = %self.prev_header_hash.to_hex(),
            prev_ledger_seq = self.prev_header.ledger_seq,
            close_time = self.close_data.close_time,
            tx_set_hash = %self.close_data.tx_set_hash().to_hex(),
            bucket_list_hash = %bucket_list_hash.to_hex(),
            tx_result_hash = %tx_result_hash.to_hex(),
            prev_total_coins = self.prev_header.total_coins,
            total_coins_delta = self.delta.total_coins_delta(),
            total_coins = total_coins,
            prev_fee_pool = self.prev_header.fee_pool,
            fee_pool_delta = self.delta.fee_pool_delta(),
            fee_pool = fee_pool,
            inflation_seq = self.prev_header.inflation_seq,
            prev_ledger_version = self.prev_header.ledger_version,
            prev_base_fee = self.prev_header.base_fee,
            prev_base_reserve = self.prev_header.base_reserve,
            prev_max_tx_set_size = self.prev_header.max_tx_set_size,
            "Header creation inputs"
        );

        // Create the new header
        let mut new_header = create_next_header(
            &self.prev_header,
            self.prev_header_hash,
            self.close_data.close_time,
            self.close_data.tx_set_hash(),
            bucket_list_hash,
            tx_result_hash,
            total_coins,
            fee_pool,
            self.prev_header.inflation_seq,
            self.close_data.stellar_value_ext.clone(),
        );

        // Apply upgrades to header fields (e.g., ledger_version, base_fee)
        self.upgrade_ctx.apply_to_header(&mut new_header);

        // Log config upgrade effects (upgrades were already applied to the delta
        // before bucket list add_batch, matching stellar-core ordering)
        if config_state_archival_changed {
            tracing::info!(
                ledger_seq = self.close_data.ledger_seq,
                "State archival settings changed via config upgrade"
            );
        }
        if config_memory_cost_params_changed {
            tracing::info!(
                ledger_seq = self.close_data.ledger_seq,
                "Memory cost params changed via config upgrade"
            );
        }

        // Also set the raw upgrades in scp_value.upgrades for correct header hash
        // The upgrades need to be XDR-encoded as UpgradeType (opaque bytes)
        let raw_upgrades: Vec<stellar_xdr::curr::UpgradeType> = self
            .close_data
            .upgrades
            .iter()
            .filter_map(|upgrade| {
                use stellar_xdr::curr::WriteXdr;
                upgrade
                    .to_xdr(stellar_xdr::curr::Limits::none())
                    .ok()
                    .and_then(|bytes| stellar_xdr::curr::UpgradeType::try_from(bytes).ok())
            })
            .collect();
        if let Ok(upgrades_vec) = raw_upgrades.try_into() {
            new_header.scp_value.upgrades = upgrades_vec;
        }

        new_header.id_pool = self.id_pool;

        // Compute header hash - add detailed XDR logging for debugging
        use stellar_xdr::curr::{Limits, WriteXdr};
        let header_xdr_bytes = new_header.to_xdr(Limits::none())?;
        let header_xdr_hex: String = header_xdr_bytes
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        tracing::debug!(
            ledger_seq = new_header.ledger_seq,
            header_xdr_len = header_xdr_bytes.len(),
            header_xdr_hex = %header_xdr_hex,
            "Full header XDR for hash debugging"
        );
        let header_hash = compute_header_hash(&new_header)?;

        Ok((new_header, header_hash))
    }

    /// Commit the ledger close: finalize state, update bucket list, build header.
    ///
    /// LEDGER_SPEC §6 defines an 8-step commit sequence:
    ///   1. Compute tx result hash
    ///   2. Apply upgrades
    ///   3. Update bucket list with delta
    ///   4. Run eviction scan (protocol 23+)
    ///   5. Compute bucket list hash
    ///   6. Build new ledger header
    ///   7. Update in-memory state (offer index, Soroban cache)
    ///   8. Emit ledger close meta
    ///
    /// Henyey combines some of these steps (e.g., bucket list update and hash
    /// computation happen together under a single write lock), but the logical
    /// ordering is preserved.
    fn commit(mut self, rss_before: u64) -> Result<LedgerCloseResult> {
        tracing::debug!(
            ledger_seq = self.close_data.ledger_seq,
            "LedgerCloseContext::commit starting"
        );

        let commit_start = std::time::Instant::now();

        // Compute transaction result hash by streaming XDR directly to the
        // hasher, avoiding a clone of all 50K results just to build a
        // TransactionResultSet wrapper.
        let tx_result_hash = {
            use sha2::{Digest, Sha256};
            use stellar_xdr::curr::{Limited, Limits, WriteXdr};

            struct Sha256Writer(Sha256);
            impl std::io::Write for Sha256Writer {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    self.0.update(buf);
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }

            let mut writer = Sha256Writer(Sha256::new());
            let mut limited = Limited::new(&mut writer, Limits::none());
            // XDR variable-length array: 4-byte length prefix + elements
            let len = self.tx_results.len() as u32;
            let ok = len.write_xdr(&mut limited).is_ok()
                && self
                    .tx_results
                    .iter()
                    .all(|r| r.write_xdr(&mut limited).is_ok());
            if ok {
                let hash: [u8; 32] = writer.0.finalize().into();
                Hash256::from(hash)
            } else {
                Hash256::ZERO
            }
        };

        // Log transaction results for debugging - helps identify tx execution differences
        tracing::debug!(
            ledger_seq = self.close_data.ledger_seq,
            tx_count = self.tx_results.len(),
            tx_result_hash = %tx_result_hash.to_hex(),
            "TX_RESULT: Transaction result hash computed"
        );

        let mut upgraded_header = self.prev_header.clone();
        self.upgrade_ctx.apply_to_header(&mut upgraded_header);
        let protocol_version = upgraded_header.ledger_version;
        let prev_version = self.prev_header.ledger_version;
        tracing::debug!(
            ledger_seq = self.close_data.ledger_seq,
            prev_protocol_version = prev_version,
            upgraded_protocol_version = protocol_version,
            "Protocol version for commit"
        );

        let (config_state_archival_changed, config_memory_cost_params_changed, upgrades_meta) =
            self.apply_upgrades_to_delta(prev_version, protocol_version)?;

        tracing::debug!(
            ledger_seq = self.close_data.ledger_seq,
            delta_count_final = self.delta.num_changes(),
            "Delta entry count before bucket list update"
        );

        // Load state archival settings BEFORE acquiring bucket list lock to avoid deadlock.
        // The snapshot's lookup_fn tries to acquire a read lock on bucket_list, which would
        // deadlock if we're already holding the write lock.
        // Parity: In stellar-core, eviction runs after config upgrades (sealLedgerTxnAndStoreInBucketsAndDB),
        // so it reads the post-upgrade StateArchival settings. We use load_state_archival_settings()
        // which checks the delta first (containing any upgrade changes) before the snapshot.
        let eviction_settings = if protocol_version_starts_from(protocol_version, ProtocolVersion::V23)
        {
            tracing::debug!(
                ledger_seq = self.close_data.ledger_seq,
                "Loading state archival settings"
            );
            let settings = self.load_state_archival_settings().unwrap_or_default();
            tracing::debug!(
                ledger_seq = self.close_data.ledger_seq,
                "Loaded state archival settings"
            );
            Some(settings)
        } else {
            None
        };

        // Categorize delta entries for bucket list update (single pass) before acquiring write lock.
        // Drains entries from the delta (moving instead of cloning), saving ~50K clone operations.
        // Metadata (fee_pool_delta, total_coins_delta) is preserved for build_and_hash_header.
        let cat = self.delta.drain_categorization_for_bucket_update();
        let init_entries = cat.init_entries;
        let mut live_entries = cat.live_entries;
        let mut dead_entries = cat.dead_keys;
        let bucket_created_count = cat.created_count;
        let bucket_updated_count = cat.updated_count;
        let bucket_deleted_count = cat.deleted_count;
        let delta_has_offers = cat.has_offers;
        let delta_has_pool_share_trustlines = cat.has_pool_share_trustlines;
        let offer_pool_changes = cat.offer_pool_changes;

        let commit_setup_us = commit_start.elapsed().as_micros() as u64;

        // Apply delta to bucket list FIRST, then compute its hash
        // This ensures the bucket_list_hash in the header matches the actual state
        tracing::debug!(
            ledger_seq = self.close_data.ledger_seq,
            "Acquiring bucket list write lock"
        );
        let (bucket_list_hash, bucket_lock_wait_us, eviction_us, soroban_state_us, add_batch_us, hot_archive_us, bg_eviction_data, evicted_meta_keys) = {
            let lock_wait_start = std::time::Instant::now();
            let mut bucket_list = self.manager.bucket_list.write();
            let bucket_lock_wait_us = lock_wait_start.elapsed().as_micros() as u64;
            tracing::debug!(
                ledger_seq = self.close_data.ledger_seq,
                "Acquired bucket list write lock"
            );

            // Filter out entries restored from hot archive that were then deleted.
            // These entries came from hot archive (not live bucket list), so deleting them
            // should NOT add them to the live bucket list's DEAD entries. The hot archive
            // restoration is handled separately via hot_archive_restored_keys.
            if !self.hot_archive_restored_keys.is_empty() {
                let restored_set: std::collections::HashSet<_> =
                    self.hot_archive_restored_keys.iter().collect();
                let before_count = dead_entries.len();
                dead_entries.retain(|key| !restored_set.contains(key));
                if dead_entries.len() != before_count {
                    tracing::debug!(
                        ledger_seq = self.close_data.ledger_seq,
                        before_count = before_count,
                        after_count = dead_entries.len(),
                        filtered_count = before_count - dead_entries.len(),
                        "Filtered hot archive restored entries from dead_entries"
                    );
                }
            }

            // Log bucket list entries for debugging hash mismatch
            tracing::debug!(
                ledger_seq = self.close_data.ledger_seq,
                init_count = init_entries.len(),
                live_count = live_entries.len(),
                dead_count = dead_entries.len(),
                "Bucket list entries from delta"
            );

            // Log first few entries for debugging
            for (i, entry) in init_entries.iter().take(5).enumerate() {
                let key = henyey_common::entry_to_key(entry);
                tracing::debug!(
                    ledger_seq = self.close_data.ledger_seq,
                    index = i,
                    key = ?key,
                    last_modified = entry.last_modified_ledger_seq,
                    "INIT entry"
                );
            }
            for (i, entry) in live_entries.iter().take(5).enumerate() {
                let key = henyey_common::entry_to_key(entry);
                tracing::debug!(
                    ledger_seq = self.close_data.ledger_seq,
                    index = i,
                    key = ?key,
                    last_modified = entry.last_modified_ledger_seq,
                    "LIVE entry"
                );
            }
            for (i, key) in dead_entries.iter().take(5).enumerate() {
                tracing::debug!(
                    ledger_seq = self.close_data.ledger_seq,
                    index = i,
                    key = ?key,
                    "DEAD entry"
                );
            }

            tracing::debug!(ledger_seq = self.close_data.ledger_seq, "Got delta entries");

            // Run incremental eviction scan for Protocol 23+
            // This must happen BEFORE applying transaction changes to match stellar-core.
            //
            // Background eviction optimization: after committing ledger N, a background
            // thread scans for entries to evict at N+1 using a bucket list snapshot.
            // When N+1 arrives here, we try to use that pre-computed result instead of
            // scanning inline. Falls back to inline scan for the first ledger, on
            // settings mismatch (config upgrade), or if the background scan failed.
            let mut archived_entries: Vec<LedgerEntry> = Vec::new();
            let mut eviction_us: u64 = 0;
            let mut evicted_meta_keys: Vec<LedgerKey> = Vec::new();

            if protocol_version_starts_from(protocol_version, ProtocolVersion::V23) {
                let hot_archive_guard = self.manager.hot_archive_bucket_list.read();
                if hot_archive_guard.is_some() {
                    drop(hot_archive_guard); // Release read lock before write operations

                    // Use pre-loaded eviction settings (loaded before bucket list lock)
                    let eviction_settings = eviction_settings.clone().unwrap_or_default();

                    // Try to use background eviction scan from previous ledger
                    let eviction_start = std::time::Instant::now();
                    let eviction_result = {
                        let pending = self.manager.pending_eviction_scan.lock().take();
                        let background_result = pending.and_then(|scan| {
                            if scan.target_ledger_seq != self.close_data.ledger_seq {
                                tracing::debug!(
                                    ledger_seq = self.close_data.ledger_seq,
                                    target = scan.target_ledger_seq,
                                    "Discarding background eviction scan: ledger mismatch"
                                );
                                return None;
                            }
                            if scan.settings != eviction_settings {
                                tracing::debug!(
                                    ledger_seq = self.close_data.ledger_seq,
                                    "Discarding background eviction scan: settings changed"
                                );
                                return None;
                            }
                            match scan.handle.join() {
                                Ok(Ok(result)) => {
                                    tracing::debug!(
                                        ledger_seq = self.close_data.ledger_seq,
                                        candidates = result.candidates.len(),
                                        bytes_scanned = result.bytes_scanned,
                                        "Using background eviction scan result"
                                    );
                                    Some(result)
                                }
                                Ok(Err(e)) => {
                                    tracing::warn!(
                                        ledger_seq = self.close_data.ledger_seq,
                                        error = %e,
                                        "Background eviction scan failed, falling back to inline"
                                    );
                                    None
                                }
                                Err(_) => {
                                    tracing::warn!(
                                        ledger_seq = self.close_data.ledger_seq,
                                        "Background eviction scan panicked, falling back to inline"
                                    );
                                    None
                                }
                            }
                        });

                        match background_result {
                            Some(result) => result,
                            None => {
                                // Inline fallback: load iterator and scan synchronously
                                let iter = load_eviction_iterator_from_bucket_list(&bucket_list)
                                    .unwrap_or_else(|| {
                                        tracing::debug!(
                                            ledger_seq = self.close_data.ledger_seq,
                                            starting_level = eviction_settings.starting_eviction_scan_level,
                                            "Creating new EvictionIterator (no entry found)"
                                        );
                                        EvictionIterator::new(
                                            eviction_settings.starting_eviction_scan_level,
                                        )
                                    });
                                bucket_list
                                    .scan_for_eviction_incremental(
                                        iter,
                                        self.close_data.ledger_seq,
                                        &eviction_settings,
                                    )
                                    .map_err(LedgerError::Bucket)?
                            }
                        }
                    };
                    let eviction_duration = eviction_start.elapsed();
                    eviction_us = eviction_duration.as_micros() as u64;

                    tracing::debug!(
                        ledger_seq = self.close_data.ledger_seq,
                        bytes_scanned = eviction_result.bytes_scanned,
                        candidates = eviction_result.candidates.len(),
                        duration_ms = eviction_duration.as_millis(),
                        "Eviction completed"
                    );

                    // Resolution phase: apply TTL filtering + max_entries limit.
                    // This matches stellar-core resolveBackgroundEvictionScan which:
                    // 1. Filters out entries whose TTL was modified by TXs
                    // 2. Evicts up to maxEntriesToArchive entries
                    // 3. Sets iterator based on whether the limit was hit
                    let modified_ttl_keys: std::collections::HashSet<LedgerKey> = init_entries
                        .iter()
                        .chain(live_entries.iter())
                        .filter_map(|entry| {
                            if let LedgerEntryData::Ttl(ttl) = &entry.data {
                                Some(LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl {
                                    key_hash: ttl.key_hash.clone(),
                                }))
                            } else {
                                None
                            }
                        })
                        .collect();

                    let bytes_scanned = eviction_result.bytes_scanned;
                    let resolved = eviction_result
                        .resolve(eviction_settings.max_entries_to_archive, &modified_ttl_keys);

                    // Capture evicted keys for LedgerCloseMeta before consuming them.
                    // Parity: LedgerCloseMetaFrame.cpp:170-187 populateEvictedEntries()
                    // adds deletedKeys (temp data + all TTL keys) and LedgerEntryKey(entry)
                    // for archived persistent entries. Our resolved.evicted_keys already
                    // contains all of these.
                    evicted_meta_keys = resolved.evicted_keys.clone();

                    dead_entries.extend(resolved.evicted_keys);
                    archived_entries = resolved.archived_entries;

                    // Log before moving end_iterator into the entry
                    tracing::debug!(
                        ledger_seq = self.close_data.ledger_seq,
                        bytes_scanned = bytes_scanned,
                        level = resolved.end_iterator.bucket_list_level,
                        is_curr = resolved.end_iterator.is_curr_bucket,
                        offset = resolved.end_iterator.bucket_file_offset,
                        "Added EvictionIterator entry to live entries"
                    );

                    // Add EvictionIterator update to live entries
                    let eviction_iter_entry = LedgerEntry {
                        last_modified_ledger_seq: self.close_data.ledger_seq,
                        data: LedgerEntryData::ConfigSetting(ConfigSettingEntry::EvictionIterator(
                            resolved.end_iterator,
                        )),
                        ext: LedgerEntryExt::V0,
                    };

                    live_entries.push(eviction_iter_entry);
                }
            }

            // Update state size window (Protocol 20+)
            // IMPORTANT: Per stellar-core, we snapshot the state size BEFORE flushing
            // the updated entries into in-memory state. So the snapshot taken at ledger N
            // will have the state size for ledger N-1. This is a protocol implementation detail.
            if protocol_version >= henyey_common::MIN_SOROBAN_PROTOCOL_VERSION {
                // Check if window entry was already added by transaction execution
                let has_window_entry = live_entries.iter().any(|e| {
                    matches!(
                        &e.data,
                        LedgerEntryData::ConfigSetting(
                            stellar_xdr::curr::ConfigSettingEntry::LiveSorobanStateSizeWindow(_)
                        )
                    )
                });

                if !has_window_entry {
                    // Check if this is a sample ledger before computing window entry
                    // Sample period is typically 64 ledgers
                    let archival_key = stellar_xdr::curr::LedgerKey::ConfigSetting(
                        stellar_xdr::curr::LedgerKeyConfigSetting {
                            config_setting_id: stellar_xdr::curr::ConfigSettingId::StateArchival,
                        },
                    );
                    let sample_period = bucket_list
                        .get(&archival_key)
                        .ok()
                        .flatten()
                        .and_then(|e| {
                            if let LedgerEntryData::ConfigSetting(
                                stellar_xdr::curr::ConfigSettingEntry::StateArchival(archival),
                            ) = e.data
                            {
                                Some(archival.live_soroban_state_size_window_sample_period)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(64); // Default to 64 if not found

                    // Only compute state size on sample ledgers
                    let is_sample_ledger =
                        sample_period > 0 && self.close_data.ledger_seq % sample_period == 0;

                    if is_sample_ledger {
                        // Use in-memory Soroban state total_size() - this is the state BEFORE
                        // this ledger's changes are applied (matching stellar-core behavior)
                        let soroban_state_size = self.manager.soroban_state.read().total_size();

                        if let Some(window_entry) =
                            crate::execution::compute_state_size_window_entry(
                                self.close_data.ledger_seq,
                                protocol_version,
                                &bucket_list,
                                soroban_state_size,
                            )
                        {
                            tracing::info!(
                                ledger_seq = self.close_data.ledger_seq,
                                soroban_state_size = soroban_state_size,
                                "Adding state size window entry to live entries (from in-memory state)"
                            );
                            live_entries.push(window_entry);
                        }
                    }
                }
            }

            // Update in-memory Soroban state with changes from this ledger.
            // This happens AFTER computing state size window (see comment above).
            let soroban_state_start = std::time::Instant::now();
            if protocol_version >= henyey_common::MIN_SOROBAN_PROTOCOL_VERSION {
                // Load rent config for accurate code size calculation
                let rent_config = self.manager.load_soroban_rent_config(&bucket_list);
                let mut soroban_state = self.manager.soroban_state.write();

                // Process init entries (creates).
                // When a hot-archive-restored entry appears as INIT in the delta but
                // still exists in IMS (with an expired TTL — pending eviction from the live
                // bucket list), process_entry_create fails with "already exists".  In that
                // case fall back to process_entry_update so the IMS entry and its TTL are
                // refreshed to the restored values, preventing stale expired-TTL data from
                // causing spurious EntryArchived errors on subsequent ledgers.
                for entry in &init_entries {
                    if soroban_state
                        .process_entry_create(entry, protocol_version, rent_config.as_ref())
                        .is_err()
                    {
                        if let Err(e) = soroban_state.process_entry_update(
                            entry,
                            protocol_version,
                            rent_config.as_ref(),
                        ) {
                            tracing::trace!(error = %e, "Failed to process init entry in soroban state");
                        }
                    }
                }

                // Process live entries (updates)
                for entry in &live_entries {
                    if let Err(e) = soroban_state.process_entry_update(
                        entry,
                        protocol_version,
                        rent_config.as_ref(),
                    ) {
                        tracing::trace!(error = %e, "Failed to process live entry in soroban state");
                    }
                }

                // Process dead entries (deletes)
                for key in &dead_entries {
                    if let Err(e) = soroban_state.process_entry_delete(key) {
                        tracing::trace!(error = %e, "Failed to process dead entry in soroban state");
                    }

                    // Remove evicted contract code from the module cache to prevent
                    // unbounded memory growth.
                    if let LedgerKey::ContractCode(cc) = key {
                        let module_cache_guard = self.manager.module_cache.read();
                        if let Some(cache) = module_cache_guard.as_ref() {
                            if cache.remove_contract(&cc.hash) {
                                tracing::debug!(
                                    hash = ?cc.hash,
                                    "Removed evicted contract code from module cache"
                                );
                            }
                        }
                    }
                }

                tracing::debug!(
                    ledger_seq = self.close_data.ledger_seq,
                    total_size = soroban_state.total_size(),
                    data_count = soroban_state.contract_data_count(),
                    code_count = soroban_state.contract_code_count(),
                    "Updated in-memory Soroban state"
                );
            }
            let soroban_state_us = soroban_state_start.elapsed().as_micros() as u64;

            // CRITICAL: Advance the bucket list through any skipped ledgers.
            // The bucket list merge algorithm depends on being called for every ledger
            // in sequence. In live mode, we may skip ledgers if there are no transactions
            // between consensus rounds. This ensures proper merge timing.
            let current_bl_ledger = bucket_list.ledger_seq();
            tracing::debug!(
                current_bl_ledger = current_bl_ledger,
                target_ledger = self.close_data.ledger_seq,
                needs_advance = current_bl_ledger < self.close_data.ledger_seq - 1,
                "Checking if bucket list advance is needed"
            );
            if current_bl_ledger < self.close_data.ledger_seq - 1 {
                let advance_from = current_bl_ledger + 1;
                let advance_to = self.close_data.ledger_seq;
                tracing::debug!(
                    current_bl_ledger = current_bl_ledger,
                    target_ledger = self.close_data.ledger_seq,
                    skipped_count = advance_to - advance_from,
                    "Advancing bucket list through empty ledgers"
                );
                bucket_list.advance_to_ledger(
                    self.close_data.ledger_seq,
                    protocol_version,
                    BucketListType::Live,
                )?;
            }

            // Log bucket list hash BEFORE add_batch
            let pre_add_batch_hash = bucket_list.hash();
            tracing::debug!(
                ledger_seq = self.close_data.ledger_seq,
                pre_add_batch_hash = %pre_add_batch_hash.to_hex(),
                init_count = init_entries.len(),
                live_count = live_entries.len(),
                dead_count = dead_entries.len(),
                "Bucket list state before add_batch"
            );

            // Detailed entry logging for debugging
            for (i, entry) in init_entries.iter().enumerate() {
                let key = henyey_common::entry_to_key(entry);
                tracing::trace!(
                    ledger_seq = self.close_data.ledger_seq,
                    idx = i,
                    entry_type = ?std::mem::discriminant(&entry.data),
                    key = ?key,
                    last_modified = entry.last_modified_ledger_seq,
                    "INIT entry"
                );
            }
            for (i, entry) in live_entries.iter().enumerate() {
                let key = henyey_common::entry_to_key(entry);
                // For ConfigSetting entries, log the data for comparison
                let config_data = match &entry.data {
                    LedgerEntryData::ConfigSetting(cs) => Some(format!("{:?}", cs)),
                    _ => None,
                };
                tracing::trace!(
                    ledger_seq = self.close_data.ledger_seq,
                    idx = i,
                    entry_type = ?std::mem::discriminant(&entry.data),
                    key = ?key,
                    last_modified = entry.last_modified_ledger_seq,
                    config_data = ?config_data,
                    "LIVE entry"
                );
            }
            for (i, key) in dead_entries.iter().enumerate() {
                tracing::trace!(
                    ledger_seq = self.close_data.ledger_seq,
                    idx = i,
                    key = ?key,
                    "DEAD entry"
                );
            }

            tracing::debug!(
                ledger_seq = self.close_data.ledger_seq,
                init_count = init_entries.len(),
                live_count = live_entries.len(),
                dead_count = dead_entries.len(),
                "Adding entries to live bucket list"
            );

            let add_batch_start = std::time::Instant::now();
            bucket_list.add_batch_unique(
                self.close_data.ledger_seq,
                protocol_version,
                BucketListType::Live,
                init_entries,
                live_entries,
                dead_entries,
            )?;
            let add_batch_us = add_batch_start.elapsed().as_micros() as u64;

            // Record completed merges in the shared merge map for deduplication.
            // This matches stellar-core's adoptFileAsBucket() -> recordMerge() flow.
            if let Some(ref merge_map) = self.manager.finished_merges {
                let completed = bucket_list.drain_completed_merges();
                if !completed.is_empty() {
                    let mut map = merge_map.write().unwrap();
                    for (key, output_hash) in completed {
                        tracing::debug!(
                            level_curr = %key.curr_hash,
                            level_snap = %key.snap_hash,
                            output = %output_hash,
                            "Recording completed merge in merge map"
                        );
                        map.record_merge(key, output_hash);
                    }
                }
            }

            let live_hash = bucket_list.hash();

            // For Protocol 23+, update hot archive and combine bucket list hashes
            let hot_archive_start = std::time::Instant::now();
            let final_hash = if protocol_version_starts_from(protocol_version, ProtocolVersion::V23) {
                let mut hot_archive_guard = self.manager.hot_archive_bucket_list.write();
                if let Some(ref mut hot_archive) = *hot_archive_guard {
                    // Advance hot archive through any skipped ledgers (same as live bucket list)
                    let current_hot_ledger = hot_archive.ledger_seq();
                    if current_hot_ledger < self.close_data.ledger_seq - 1 {
                        tracing::debug!(
                            current_hot_ledger = current_hot_ledger,
                            target_ledger = self.close_data.ledger_seq,
                            skipped_count = self.close_data.ledger_seq - current_hot_ledger - 1,
                            "Advancing hot archive bucket list through empty ledgers"
                        );
                        hot_archive
                            .advance_to_ledger(self.close_data.ledger_seq, protocol_version)?;
                    }

                    // Add archived entries to hot archive bucket list
                    // Must call add_batch even with empty entries to maintain spill consistency
                    // restored_keys contains entries restored via RestoreFootprint or InvokeHostFunction
                    hot_archive.add_batch(
                        self.close_data.ledger_seq,
                        protocol_version,
                        archived_entries,
                        std::mem::take(&mut self.hot_archive_restored_keys),
                    )?;

                    use sha2::{Digest, Sha256};
                    let hot_hash = hot_archive.hash();

                    let mut hasher = Sha256::new();
                    hasher.update(live_hash.as_bytes());
                    hasher.update(hot_hash.as_bytes());
                    let result = hasher.finalize();
                    let mut bytes = [0u8; 32];
                    bytes.copy_from_slice(&result);
                    let combined_hash = Hash256::from_bytes(bytes);
                    combined_hash
                } else {
                    // No hot archive bucket list available, use live hash only
                    // This shouldn't happen for Protocol 23+ but fall back gracefully
                    tracing::warn!(
                        ledger_seq = self.close_data.ledger_seq,
                        protocol_version = protocol_version,
                        live_hash = %live_hash.to_hex(),
                        "HOT ARCHIVE IS NONE for Protocol 23+! Using live hash only - this WILL cause hash mismatch!"
                    );
                    live_hash
                }
            } else {
                live_hash
            };
            let hot_archive_us = hot_archive_start.elapsed().as_micros() as u64;

            // Prepare data for background eviction scan (snapshot while we hold the lock)
            let bg_eviction_data = if protocol_version_starts_from(
                protocol_version,
                ProtocolVersion::V23,
            ) {
                eviction_settings.map(|settings| {
                    let snapshot =
                        BucketListSnapshot::new(&bucket_list, self.prev_header.clone());
                    let iter = load_eviction_iterator_from_bucket_list(&bucket_list)
                        .unwrap_or_else(|| {
                            EvictionIterator::new(settings.starting_eviction_scan_level)
                        });
                    (snapshot, iter, settings)
                })
            } else {
                None
            };

            (final_hash, bucket_lock_wait_us, eviction_us, soroban_state_us, add_batch_us, hot_archive_us, bg_eviction_data, evicted_meta_keys)
        };

        // Start background eviction scan for the next ledger.
        // The scan runs on a snapshot of the bucket list (taken above while the write
        // lock was held), so it doesn't interfere with subsequent operations.
        if let Some((snapshot, iter, settings)) = bg_eviction_data {
            let target_ledger_seq = self.close_data.ledger_seq + 1;
            let settings_clone = settings.clone();
            let handle = std::thread::spawn(move || {
                snapshot.scan_for_eviction_incremental(iter, target_ledger_seq, &settings_clone)
            });
            *self.manager.pending_eviction_scan.lock() = Some(PendingEvictionScan {
                handle,
                target_ledger_seq,
                settings,
            });
        }

        let header_start = std::time::Instant::now();
        let (new_header, header_hash) = self.build_and_hash_header(
            bucket_list_hash,
            tx_result_hash,
            config_state_archival_changed,
            config_memory_cost_params_changed,
        )?;
        let header_us = header_start.elapsed().as_micros() as u64;

        // Record stats (counts were already computed during categorize_for_bucket_update)
        self.stats
            .record_entry_changes(bucket_created_count, bucket_updated_count, bucket_deleted_count);

        // Commit to manager
        let commit_close_start = std::time::Instant::now();
        self.manager
            .commit_close(offer_pool_changes, new_header.clone(), header_hash, delta_has_offers, delta_has_pool_share_trustlines)?;
        let commit_close_us = commit_close_start.elapsed().as_micros() as u64;

        // If protocol upgraded to a new major version, rebuild the module cache.
        // Transactions in THIS ledger ran under prev_version; the NEXT ledger
        // needs a cache matching the new protocol version.
        if needs_upgrade_to_version(ProtocolVersion::V25, prev_version, protocol_version) {
            self.manager.rebuild_module_cache(protocol_version);
        }

        self.stats
            .set_close_time(self.start.elapsed().as_millis() as u64);

        // Describe the StellarValueExt for logging
        let stellar_value_ext_desc = match &new_header.scp_value.ext {
            stellar_xdr::curr::StellarValueExt::Basic => "Basic".to_string(),
            stellar_xdr::curr::StellarValueExt::Signed(_) => "Signed".to_string(),
        };

        info!(
            ledger_seq = new_header.ledger_seq,
            tx_count = self.stats.tx_count,
            close_time_ms = self.stats.close_time_ms,
            computed_hash = %header_hash.to_hex(),
            bucket_list_hash = %bucket_list_hash.to_hex(),
            tx_result_hash = %tx_result_hash.to_hex(),
            total_coins = new_header.total_coins,
            fee_pool = new_header.fee_pool,
            close_time = new_header.scp_value.close_time.0,
            "Ledger closed"
        );
        debug!(
            ledger_seq = new_header.ledger_seq,
            tx_set_hash = %Hash256::from(new_header.scp_value.tx_set_hash.0).to_hex(),
            upgrades_count = new_header.scp_value.upgrades.len(),
            stellar_value_ext = %stellar_value_ext_desc,
            prev_header_hash = %self.prev_header_hash.to_hex(),
            skip_list_0 = %Hash256::from_bytes(new_header.skip_list[0].0).to_hex(),
            skip_list_1 = %Hash256::from_bytes(new_header.skip_list[1].0).to_hex(),
            skip_list_2 = %Hash256::from_bytes(new_header.skip_list[2].0).to_hex(),
            skip_list_3 = %Hash256::from_bytes(new_header.skip_list[3].0).to_hex(),
            id_pool = new_header.id_pool,
            inflation_seq = new_header.inflation_seq,
            base_fee = new_header.base_fee,
            base_reserve = new_header.base_reserve,
            max_tx_set_size = new_header.max_tx_set_size,
            "Ledger closed details"
        );

        // Snapshot per-bucket cache stats (aggregated across all buckets) and reset counters
        let cache_perf = {
            let bl = self.manager.bucket_list.read();
            let stats = bl.aggregate_cache_stats();
            if stats.active {
                tracing::debug!(
                    ledger_seq = new_header.ledger_seq,
                    cache_entries = stats.entry_count,
                    cache_bytes = stats.size_bytes,
                    hits = stats.hits,
                    misses = stats.misses,
                    hit_rate = format!("{:.1}%", stats.hit_rate * 100.0),
                    "Per-bucket cache stats"
                );
            }
            crate::close::CachePerfStats {
                entry_count: stats.entry_count,
                size_bytes: stats.size_bytes,
                hits: stats.hits,
                misses: stats.misses,
                hit_rate: stats.hit_rate,
            }
        };

        // Compute average Soroban state size from the LiveSorobanStateSizeWindow.
        // Parity: NetworkConfig.cpp:1812 — average of all window entries.
        let avg_soroban_state_size = if protocol_version >= henyey_common::MIN_SOROBAN_PROTOCOL_VERSION {
            let bl = self.manager.bucket_list.read();
            let window_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
                config_setting_id: ConfigSettingId::LiveSorobanStateSizeWindow,
            });
            bl.get(&window_key)
                .ok()
                .flatten()
                .and_then(|entry| {
                    if let LedgerEntryData::ConfigSetting(
                        ConfigSettingEntry::LiveSorobanStateSizeWindow(window),
                    ) = &entry.data
                    {
                        if window.is_empty() {
                            return Some(0u64);
                        }
                        let sum: u64 = window.iter().copied().sum();
                        Some(sum / window.len() as u64)
                    } else {
                        None
                    }
                })
                .unwrap_or(0)
        } else {
            0
        };

        let meta_start = std::time::Instant::now();
        // Move tx_set and scp_history out of close_data to avoid cloning 50K envelopes
        let empty_tx_set = TransactionSetVariant::Classic(TransactionSet {
            previous_ledger_hash: Hash([0; 32]),
            txs: Default::default(),
        });
        let tx_set = std::mem::replace(&mut self.close_data.tx_set, empty_tx_set);
        let scp_history = std::mem::take(&mut self.close_data.scp_history);
        let meta = build_ledger_close_meta(
            tx_set,
            scp_history,
            &new_header,
            header_hash,
            std::mem::take(&mut self.tx_result_metas),
            evicted_meta_keys,
            avg_soroban_state_size,
            upgrades_meta,
            self.manager.config.emit_ledger_close_meta_ext_v1,
            self.soroban_fee_write_1kb,
        );
        let meta_us = meta_start.elapsed().as_micros() as u64;

        // Emit single summary timing line for performance analysis
        let total_us = self.start.elapsed().as_micros() as u64;
        debug!(
            ledger_seq = new_header.ledger_seq,
            total_us,
            begin_close_us = self.timing_begin_close_us,
            tx_exec_us = self.timing_tx_exec_us,
            classic_exec_us = self.timing_classic_exec_us,
            soroban_exec_us = self.timing_soroban_exec_us,
            commit_setup_us,
            bucket_lock_wait_us,
            eviction_us,
            soroban_state_us,
            add_batch_us,
            hot_archive_us,
            header_us,
            commit_close_us,
            meta_us,
            tx_count = self.stats.tx_count,
            "Ledger close timing"
        );

        // Periodic memory report (every 64 ledgers, ~5 minutes)
        if new_header.ledger_seq % 64 == 0 {
            let report = self.manager.build_memory_report(new_header.ledger_seq);
            report.log();
        }

        let rss_after = get_rss_bytes();

        // Sort tx_perf by exec_us descending (worst offenders first)
        let mut tx_timings = self.tx_perf;
        tx_timings.sort_by(|a, b| b.exec_us.cmp(&a.exec_us));

        let perf = crate::close::LedgerClosePerf {
            begin_close_us: self.timing_begin_close_us,
            tx_exec_us: self.timing_tx_exec_us,
            classic_exec_us: self.timing_classic_exec_us,
            soroban_exec_us: self.timing_soroban_exec_us,
            prepare_us: self.timing_prepare_us,
            config_load_us: self.timing_config_load_us,
            executor_setup_us: self.timing_executor_setup_us,
            fee_pre_deduct_us: self.timing_fee_pre_deduct_us,
            post_exec_us: self.timing_post_exec_us,
            commit_setup_us,
            bucket_lock_wait_us,
            eviction_us,
            soroban_state_us,
            add_batch_us,
            hot_archive_us,
            header_us,
            commit_close_us,
            meta_us,
            total_us,
            tx_timings,
            tx_count: self.stats.tx_count,
            cache: cache_perf,
            rss_before_bytes: rss_before,
            rss_after_bytes: rss_after,
        };

        Ok(LedgerCloseResult::new(new_header, header_hash)
            .with_tx_results(self.tx_results)
            .with_meta(meta)
            .with_perf(perf))
    }
}

fn build_generalized_tx_set_owned(tx_set: TransactionSetVariant) -> GeneralizedTransactionSet {
    match tx_set {
        TransactionSetVariant::Generalized(set) => set,
        TransactionSetVariant::Classic(set) => {
            let component = TxSetComponent::TxsetCompTxsMaybeDiscountedFee(
                TxSetComponentTxsMaybeDiscountedFee {
                    base_fee: None,
                    txs: set.txs,
                },
            );
            let phase = TransactionPhase::V0(vec![component].try_into().unwrap_or_default());
            GeneralizedTransactionSet::V1(TransactionSetV1 {
                previous_ledger_hash: set.previous_ledger_hash,
                phases: vec![phase].try_into().unwrap_or_default(),
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_ledger_close_meta(
    tx_set_variant: TransactionSetVariant,
    scp_history: Vec<ScpHistoryEntry>,
    header: &LedgerHeader,
    header_hash: Hash256,
    tx_result_metas: Vec<TransactionResultMetaV1>,
    evicted_keys: Vec<LedgerKey>,
    total_byte_size_of_live_soroban_state: u64,
    upgrades_processing: Vec<UpgradeEntryMeta>,
    emit_ext_v1: bool,
    soroban_fee_write_1kb: i64,
) -> LedgerCloseMeta {
    let ledger_header = LedgerHeaderHistoryEntry {
        hash: Hash::from(header_hash),
        header: header.clone(),
        ext: LedgerHeaderHistoryEntryExt::V0,
    };

    let tx_set = build_generalized_tx_set_owned(tx_set_variant);

    // Build the meta extension. When EMIT_LEDGER_CLOSE_META_EXT_V1 is enabled,
    // include sorobanFeeWrite1KB (the flat-rate write fee per 1KB, i.e.
    // stellar-core's feeRent1KB()). This matches stellar-core's
    // LedgerCloseMetaFrame::setNetworkConfiguration().
    let ext = if emit_ext_v1 {
        LedgerCloseMetaExt::V1(LedgerCloseMetaExtV1 {
            ext: ExtensionPoint::V0,
            soroban_fee_write1_kb: soroban_fee_write_1kb,
        })
    } else {
        LedgerCloseMetaExt::V0
    };

    // NOTE: The spec (LEDGER_SPEC §8.1) branches on `initialLedgerVers` to
    // select V0/V1/V2 meta format. Henyey supports protocol 24+ only, which
    // always uses V2, so we unconditionally produce V2 meta here.
    LedgerCloseMeta::V2(LedgerCloseMetaV2 {
        ext,
        ledger_header,
        tx_set,
        tx_processing: tx_result_metas.try_into().unwrap_or_default(),
        upgrades_processing: upgrades_processing.try_into().unwrap_or_default(),
        scp_info: scp_history.try_into().unwrap_or_default(),
        total_byte_size_of_live_soroban_state,
        evicted_keys: evicted_keys.try_into().unwrap_or_default(),
    })
}

/// Create a genesis ledger header.
fn create_genesis_header() -> LedgerHeader {
    LedgerHeader {
        ledger_version: 0,
        previous_ledger_hash: Hash([0u8; 32]),
        scp_value: stellar_xdr::curr::StellarValue {
            tx_set_hash: Hash([0u8; 32]),
            close_time: stellar_xdr::curr::TimePoint(0),
            upgrades: stellar_xdr::curr::VecM::default(),
            ext: stellar_xdr::curr::StellarValueExt::Basic,
        },
        tx_set_result_hash: Hash([0u8; 32]),
        bucket_list_hash: Hash([0u8; 32]),
        // Spec: LEDGER_SPEC §13.1 — genesis ledger constants.
        ledger_seq: 1,
        total_coins: 1_000_000_000_000_000_000, // 100 billion XLM in stroops
        fee_pool: 0,
        inflation_seq: 0,
        id_pool: 0,
        base_fee: 100,
        base_reserve: 100_000_000, // 10 XLM in stroops
        max_tx_set_size: 100,
        skip_list: std::array::from_fn(|_| Hash([0u8; 32])),
        ext: stellar_xdr::curr::LedgerHeaderExt::V0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        Asset, ContractDataDurability, ContractDataEntry, ContractId, ExtensionPoint,
        LedgerScpMessages, OfferEntry, OfferEntryExt, Price, ScAddress, ScpHistoryEntry,
        ScpHistoryEntryV0, ScVal, TransactionSet, TtlEntry, WriteXdr,
    };

    // Note: These tests require proper mocking of BucketManager and Database
    // For now they are placeholder tests

    const TEST_PROTOCOL: u32 = 25;

    fn make_account_id(bytes: [u8; 32]) -> AccountId {
        AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(bytes),
        ))
    }

    fn make_offer_entry(offer_id: i64, seller_bytes: [u8; 32]) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Offer(OfferEntry {
                seller_id: make_account_id(seller_bytes),
                offer_id,
                selling: Asset::Native,
                buying: Asset::Native,
                amount: 1000,
                price: Price { n: 1, d: 1 },
                flags: 0,
                ext: OfferEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_contract_data_entry(key_bytes: [u8; 32]) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: ScAddress::Contract(ContractId(Hash([1u8; 32]))),
                key: ScVal::Bytes(stellar_xdr::curr::ScBytes(
                    key_bytes.to_vec().try_into().unwrap(),
                )),
                durability: ContractDataDurability::Persistent,
                val: ScVal::I32(42),
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    fn make_ttl_entry(key_hash_bytes: [u8; 32], live_until: u32) -> LedgerEntry {
        LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Ttl(TtlEntry {
                key_hash: Hash(key_hash_bytes),
                live_until_ledger_seq: live_until,
            }),
            ext: LedgerEntryExt::V0,
        }
    }

    // ---- Parallel scan tests ----

    #[test]
    fn test_scan_empty_bucket_list() {
        let bl = BucketList::new();
        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2);
        assert!(result.offers.is_empty());
        assert!(result.soroban_state.is_empty());
    }

    #[test]
    fn test_scan_offers_from_bucket_list() {
        let mut bl = BucketList::new();
        let offer1 = make_offer_entry(1, [1u8; 32]);
        let offer2 = make_offer_entry(2, [2u8; 32]);
        bl.add_batch(1, TEST_PROTOCOL, BucketListType::Live, vec![offer1, offer2], vec![], vec![])
            .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2);
        assert_eq!(result.offers.len(), 2);
        assert!(result.offers.contains_key(&1));
        assert!(result.offers.contains_key(&2));
    }

    #[test]
    fn test_scan_contract_data_from_bucket_list() {
        let mut bl = BucketList::new();
        let cd1 = make_contract_data_entry([10u8; 32]);
        let cd2 = make_contract_data_entry([20u8; 32]);
        bl.add_batch(1, TEST_PROTOCOL, BucketListType::Live, vec![cd1, cd2], vec![], vec![])
            .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2);
        assert_eq!(result.soroban_state.contract_data_count(), 2);
    }

    #[test]
    fn test_scan_ttl_entries_from_bucket_list() {
        let mut bl = BucketList::new();
        let ttl = make_ttl_entry([30u8; 32], 1000);
        bl.add_batch(1, TEST_PROTOCOL, BucketListType::Live, vec![ttl], vec![], vec![])
            .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2);
        // TTL may or may not be added depending on whether it has a matching contract entry;
        // the important thing is no panic during parallel scan.
        assert!(result.soroban_state.contract_data_count() == 0);
    }

    #[test]
    fn test_scan_mixed_entry_types() {
        let mut bl = BucketList::new();
        let offer = make_offer_entry(42, [1u8; 32]);
        let cd = make_contract_data_entry([10u8; 32]);
        let ttl = make_ttl_entry([30u8; 32], 500);

        bl.add_batch(
            1, TEST_PROTOCOL, BucketListType::Live,
            vec![offer, cd, ttl], vec![], vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2);
        assert_eq!(result.offers.len(), 1);
        assert!(result.offers.contains_key(&42));
        assert_eq!(result.soroban_state.contract_data_count(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_scan_dead_entries_shadow() {
        // Add an offer, then delete it in a later ledger.
        // The dead entry should shadow the live one.
        let mut bl = BucketList::new();
        let offer = make_offer_entry(99, [5u8; 32]);
        bl.add_batch(1, TEST_PROTOCOL, BucketListType::Live, vec![offer], vec![], vec![])
            .unwrap();

        let dead_key = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: make_account_id([5u8; 32]),
            offer_id: 99,
        });
        bl.add_batch(2, TEST_PROTOCOL, BucketListType::Live, vec![], vec![], vec![dead_key])
            .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2);
        assert!(result.offers.is_empty(), "dead entry should shadow the live offer");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_scan_newer_level_shadows_older() {
        // Add entries at different ledgers so they end up at different levels.
        // Lower-numbered levels (newer data) should shadow higher ones.
        let mut bl = BucketList::new();

        // Add offer at ledger 1 (will be in a higher level after more adds)
        let old_offer = make_offer_entry(1, [1u8; 32]);
        bl.add_batch(1, TEST_PROTOCOL, BucketListType::Live, vec![old_offer], vec![], vec![])
            .unwrap();

        // Modify the same offer at ledger 2 (more recent → lower level)
        let mut new_offer = make_offer_entry(1, [1u8; 32]);
        if let LedgerEntryData::Offer(ref mut o) = new_offer.data {
            o.amount = 9999;
        }
        bl.add_batch(2, TEST_PROTOCOL, BucketListType::Live, vec![], vec![new_offer.clone()], vec![])
            .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2);
        assert_eq!(result.offers.len(), 1);
        // The newer version (amount=9999) should win
        if let LedgerEntryData::Offer(ref o) = result.offers[&1].data {
            assert_eq!(o.amount, 9999, "newer entry should shadow older entry");
        } else {
            panic!("expected offer entry");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_scan_many_entries_across_levels() {
        // Add enough entries across multiple ledgers to populate several levels.
        let mut bl = BucketList::new();
        let num_offers = 50;
        for i in 0..num_offers {
            let offer = make_offer_entry(i as i64, {
                let mut b = [0u8; 32];
                b[0] = (i & 0xff) as u8;
                b[1] = ((i >> 8) & 0xff) as u8;
                b
            });
            bl.add_batch(
                (i + 1) as u32, TEST_PROTOCOL, BucketListType::Live,
                vec![offer], vec![], vec![],
            )
            .unwrap();
        }

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2);
        assert_eq!(result.offers.len(), num_offers, "all offers should be found");
    }

    #[test]
    fn test_scan_pre_soroban_protocol_only_offers() {
        // With a protocol version below MIN_SOROBAN_PROTOCOL_VERSION,
        // only offers should be scanned (no contract data, TTLs, etc.)
        let pre_soroban_protocol = 19;
        let mut bl = BucketList::new();
        let offer = make_offer_entry(1, [1u8; 32]);
        let cd = make_contract_data_entry([10u8; 32]);
        bl.add_batch(
            1, pre_soroban_protocol, BucketListType::Live,
            vec![offer, cd], vec![], vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, pre_soroban_protocol, 2);
        assert_eq!(result.offers.len(), 1);
        assert_eq!(result.soroban_state.contract_data_count(), 0);
        assert!(result.module_cache.is_none());
    }

    #[test]
    fn test_scan_single_level_intra_level_dedup() {
        // When the same key appears in both curr and snap of a single level,
        // curr should shadow snap.
        use henyey_bucket::Bucket;

        let entry_v1 = make_offer_entry(1, [1u8; 32]);
        let mut entry_v2 = make_offer_entry(1, [1u8; 32]);
        if let LedgerEntryData::Offer(ref mut o) = entry_v2.data {
            o.amount = 5555;
        }

        // curr has v2, snap has v1 → v2 should win
        let curr = Bucket::from_entries(vec![BucketEntry::Liveentry(entry_v2.clone())]).unwrap();
        let snap = Bucket::from_entries(vec![BucketEntry::Liveentry(entry_v1.clone())]).unwrap();

        let mc: Option<Arc<PersistentModuleCache>> = None;
        let result = scan_single_level(&curr, &snap, true, &mc, TEST_PROTOCOL);

        assert_eq!(result.entries.len(), 1);
        let key = result.entries.keys().next().unwrap();
        let entry = &result.entries[key];
        if let LedgerEntryData::Offer(ref o) = entry.data {
            assert_eq!(o.amount, 5555, "curr should shadow snap within a level");
        } else {
            panic!("expected offer entry");
        }
    }

    #[test]
    fn test_scan_single_level_dead_excludes_entry() {
        // A dead entry in curr should be tracked (for cross-level shadowing)
        // but not included in the result entries.
        use henyey_bucket::Bucket;

        let entry = make_offer_entry(1, [1u8; 32]);
        let dead_key = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: make_account_id([1u8; 32]),
            offer_id: 1,
        });

        // curr has a dead entry, snap has the live entry
        let curr = Bucket::from_entries(vec![BucketEntry::Deadentry(dead_key)]).unwrap();
        let snap = Bucket::from_entries(vec![BucketEntry::Liveentry(entry)]).unwrap();

        let mc: Option<Arc<PersistentModuleCache>> = None;
        let result = scan_single_level(&curr, &snap, true, &mc, TEST_PROTOCOL);

        // The dead entry shadows the live one → no entries in result
        assert!(result.entries.is_empty(), "dead entry should shadow live entry in snap");
    }

    #[test]
    fn test_merge_level_results_cross_level_dedup() {
        // Level 0 entry should shadow level 1 entry with the same key.
        let entry_v1 = make_offer_entry(1, [1u8; 32]);
        let mut entry_v2 = make_offer_entry(1, [1u8; 32]);
        if let LedgerEntryData::Offer(ref mut o) = entry_v2.data {
            o.amount = 7777;
        }
        let key = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: make_account_id([1u8; 32]),
            offer_id: 1,
        });

        // Level 0 has v2 (newer), level 1 has v1 (older)
        let level0 = LevelScanResult {
            entries: [(key.clone(), entry_v2)].into_iter().collect(),
            ttl_entries: HashMap::new(),
            dead_keys: HashSet::new(),
            dead_ttl_keys: HashSet::new(),
        };
        let level1 = LevelScanResult {
            entries: [(key.clone(), entry_v1)].into_iter().collect(),
            ttl_entries: HashMap::new(),
            dead_keys: HashSet::new(),
            dead_ttl_keys: HashSet::new(),
        };

        let result = merge_level_results(
            vec![level0, level1],
            None,
            TEST_PROTOCOL,
            &None,
        );

        assert_eq!(result.offers.len(), 1);
        if let LedgerEntryData::Offer(ref o) = result.offers[&1].data {
            assert_eq!(o.amount, 7777, "level 0 should shadow level 1");
        } else {
            panic!("expected offer entry");
        }
    }

    #[test]
    fn test_merge_level_results_distinct_entries() {
        // Entries with different keys across levels should all be included.
        let offer1 = make_offer_entry(1, [1u8; 32]);
        let offer2 = make_offer_entry(2, [2u8; 32]);
        let key1 = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: make_account_id([1u8; 32]),
            offer_id: 1,
        });
        let key2 = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: make_account_id([2u8; 32]),
            offer_id: 2,
        });

        let level0 = LevelScanResult {
            entries: [(key1, offer1)].into_iter().collect(),
            ttl_entries: HashMap::new(),
            dead_keys: HashSet::new(),
            dead_ttl_keys: HashSet::new(),
        };
        let level1 = LevelScanResult {
            entries: [(key2, offer2)].into_iter().collect(),
            ttl_entries: HashMap::new(),
            dead_keys: HashSet::new(),
            dead_ttl_keys: HashSet::new(),
        };

        let result = merge_level_results(vec![level0, level1], None, TEST_PROTOCOL, &None);
        assert_eq!(result.offers.len(), 2);
    }

    #[test]
    fn test_merge_level_results_dead_key_shadows_older_level() {
        // Regression test: a dead entry at a lower (newer) level must prevent
        // a live entry at a higher (older) level from appearing in the result.
        // This was the root cause of the soroban_state_size inflation bug.
        let offer = make_offer_entry(1, [1u8; 32]);
        let key = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: make_account_id([1u8; 32]),
            offer_id: 1,
        });

        // Level 0 (newer): has the dead key, no live entries
        let level0 = LevelScanResult {
            entries: HashMap::new(),
            ttl_entries: HashMap::new(),
            dead_keys: [key.clone()].into_iter().collect(),
            dead_ttl_keys: HashSet::new(),
        };
        // Level 1 (older): has the live entry that should be shadowed
        let level1 = LevelScanResult {
            entries: [(key, offer)].into_iter().collect(),
            ttl_entries: HashMap::new(),
            dead_keys: HashSet::new(),
            dead_ttl_keys: HashSet::new(),
        };

        let result = merge_level_results(vec![level0, level1], None, TEST_PROTOCOL, &None);
        assert!(
            result.offers.is_empty(),
            "dead key at level 0 should shadow live entry at level 1"
        );
    }

    #[test]
    fn test_scan_offer_secondary_index() {
        let mut bl = BucketList::new();
        let seller_bytes = [7u8; 32];
        let offer = make_offer_entry(10, seller_bytes);
        bl.add_batch(1, TEST_PROTOCOL, BucketListType::Live, vec![offer], vec![], vec![])
            .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2);
        assert_eq!(result.offers.len(), 1);
        assert!(result.offers.contains_key(&10));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_scan_thread_count_1_matches_thread_count_2() {
        // Verify that scan_thread_count=1 (sequential) and scan_thread_count=2 (parallel)
        // produce identical results for a bucket list with entries across multiple levels.
        let mut bl = BucketList::new();
        let num_offers = 20;
        for i in 0..num_offers {
            let offer = make_offer_entry(i as i64, {
                let mut b = [0u8; 32];
                b[0] = (i & 0xff) as u8;
                b
            });
            bl.add_batch(
                (i + 1) as u32, TEST_PROTOCOL, BucketListType::Live,
                vec![offer], vec![], vec![],
            )
            .unwrap();
        }

        let result1 = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1);
        let result2 = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2);

        assert_eq!(result1.offers.len(), result2.offers.len());
        for (id, entry) in &result1.offers {
            assert!(result2.offers.contains_key(id), "parallel result missing offer {id}");
            if let (LedgerEntryData::Offer(ref o1), LedgerEntryData::Offer(ref o2)) =
                (&entry.data, &result2.offers[id].data)
            {
                assert_eq!(o1.amount, o2.amount, "offer {id} amount mismatch");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_scan_dead_entries_shadow_with_thread_count_1() {
        // Dead-key shadowing test with scan_thread_count=1 (exercises the sequential path).
        let mut bl = BucketList::new();
        let offer = make_offer_entry(77, [7u8; 32]);
        bl.add_batch(1, TEST_PROTOCOL, BucketListType::Live, vec![offer], vec![], vec![])
            .unwrap();

        let dead_key = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: make_account_id([7u8; 32]),
            offer_id: 77,
        });
        bl.add_batch(2, TEST_PROTOCOL, BucketListType::Live, vec![], vec![], vec![dead_key])
            .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1);
        assert!(result.offers.is_empty(), "dead entry should shadow live offer (thread_count=1)");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_scan_level_pairs_for_caches_matches_scan_bucket_list() {
        // scan_level_pairs_for_caches (used by the overlapped startup path) must
        // produce results identical to scan_bucket_list_for_caches for the same data.
        let mut bl = BucketList::new();

        // Populate with offers and contract data across multiple ledgers so
        // entries end up at different levels.
        for i in 1u32..=8 {
            let offer = make_offer_entry(i as i64, [i as u8; 32]);
            bl.add_batch(
                i,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![offer],
                vec![],
                vec![],
            )
            .unwrap();
        }

        // Scan via the BucketList reference path
        let expected = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2);

        // Extract pairs and scan via scan_level_pairs_for_caches
        let level_pairs: Vec<(Arc<henyey_bucket::Bucket>, Arc<henyey_bucket::Bucket>)> = bl
            .levels()
            .iter()
            .map(|l| (l.curr.clone(), l.snap.clone()))
            .collect();
        let actual = scan_level_pairs_for_caches(level_pairs, TEST_PROTOCOL, 2);

        assert_eq!(
            actual.offers.len(),
            expected.offers.len(),
            "offer count must match between scan_level_pairs_for_caches and scan_bucket_list_for_caches"
        );

        // Same offer IDs must be present
        for (offer_id, _) in &expected.offers {
            assert!(
                actual.offers.contains_key(offer_id),
                "offer {} missing from scan_level_pairs_for_caches result",
                offer_id
            );
        }

        assert_eq!(
            actual.soroban_state.contract_data_count(),
            expected.soroban_state.contract_data_count(),
            "contract data count must match"
        );
    }

    #[test]
    fn test_genesis_header() {
        let header = create_genesis_header();
        // Validate ALL genesis header fields per LEDGER_SPEC §13.1.
        assert_eq!(header.ledger_version, 0);
        assert_eq!(header.previous_ledger_hash, Hash([0u8; 32]));
        assert_eq!(header.scp_value.tx_set_hash, Hash([0u8; 32]));
        assert_eq!(header.scp_value.close_time.0, 0);
        assert_eq!(header.scp_value.upgrades.len(), 0);
        assert_eq!(header.tx_set_result_hash, Hash([0u8; 32]));
        assert_eq!(header.bucket_list_hash, Hash([0u8; 32]));
        assert_eq!(header.ledger_seq, 1);
        assert_eq!(header.total_coins, 1_000_000_000_000_000_000);
        assert_eq!(header.fee_pool, 0);
        assert_eq!(header.inflation_seq, 0);
        assert_eq!(header.id_pool, 0);
        assert_eq!(header.base_fee, 100);
        assert_eq!(header.base_reserve, 100_000_000);
        assert_eq!(header.max_tx_set_size, 100);
        assert_eq!(header.skip_list.len(), 4);
        for skip in &header.skip_list {
            assert_eq!(*skip, Hash([0u8; 32]));
        }
    }

    /// Parity: LedgerTests.cpp:15 "cannot close ledger with unsupported ledger version"
    /// Tests that begin_close accepts the current protocol version.
    #[test]
    fn test_close_with_current_protocol_version() {
        use henyey_common::protocol::CURRENT_LEDGER_PROTOCOL_VERSION;

        let manager = LedgerManager::new(
            "Test SDF Network ; September 2015".to_string(),
            LedgerManagerConfig {
                validate_bucket_hash: false,
                ..Default::default()
            },
        );

        let mut header = create_genesis_header();
        header.ledger_seq = 1;
        header.ledger_version = CURRENT_LEDGER_PROTOCOL_VERSION;

        let bucket_list = henyey_bucket::BucketList::new();
        let hot_archive_bucket_list = henyey_bucket::HotArchiveBucketList::new();
        let header_hash = crate::compute_header_hash(&header).expect("hash");

        manager
            .initialize(
                bucket_list,
                hot_archive_bucket_list,
                header.clone(),
                header_hash,
            )
            .expect("initialization should succeed");

        // begin_close at CURRENT version should pass the version check
        let close_data = LedgerCloseData::new(
            2,
            TransactionSetVariant::Classic(TransactionSet {
                previous_ledger_hash: header_hash.into(),
                txs: VecM::default(),
            }),
            1,
            header_hash,
        );
        // Should not panic — may fail for other reasons but NOT version
        let _result = manager.begin_close(close_data);
    }

    /// Parity: LedgerTests.cpp:15 "cannot close ledger with unsupported ledger version"
    /// Tests that begin_close panics when protocol version exceeds max supported.
    #[test]
    #[should_panic(expected = "unsupported protocol version")]
    fn test_close_panics_with_protocol_version_too_high() {
        use henyey_common::protocol::CURRENT_LEDGER_PROTOCOL_VERSION;

        let manager = LedgerManager::new(
            "Test SDF Network ; September 2015".to_string(),
            LedgerManagerConfig {
                validate_bucket_hash: false,
                ..Default::default()
            },
        );

        let mut header = create_genesis_header();
        header.ledger_seq = 1;
        header.ledger_version = CURRENT_LEDGER_PROTOCOL_VERSION;

        let bucket_list = henyey_bucket::BucketList::new();
        let hot_archive_bucket_list = henyey_bucket::HotArchiveBucketList::new();
        let header_hash = crate::compute_header_hash(&header).expect("hash");

        manager
            .initialize(
                bucket_list,
                hot_archive_bucket_list,
                header.clone(),
                header_hash,
            )
            .expect("initialization should succeed");

        // Set version beyond max supported
        manager.set_header_version_for_test(CURRENT_LEDGER_PROTOCOL_VERSION + 1);

        let close_data = LedgerCloseData::new(
            2,
            TransactionSetVariant::Classic(TransactionSet {
                previous_ledger_hash: header_hash.into(),
                txs: VecM::default(),
            }),
            1,
            header_hash,
        );
        // This should panic
        let _result = manager.begin_close(close_data);
    }

    /// Tests that begin_close panics when protocol version is below min supported.
    #[test]
    #[should_panic(expected = "unsupported protocol version")]
    fn test_close_panics_with_protocol_version_too_low() {
        use henyey_common::protocol::MIN_LEDGER_PROTOCOL_VERSION;

        let manager = LedgerManager::new(
            "Test SDF Network ; September 2015".to_string(),
            LedgerManagerConfig {
                validate_bucket_hash: false,
                ..Default::default()
            },
        );

        let mut header = create_genesis_header();
        header.ledger_seq = 1;
        header.ledger_version = MIN_LEDGER_PROTOCOL_VERSION;

        let bucket_list = henyey_bucket::BucketList::new();
        let hot_archive_bucket_list = henyey_bucket::HotArchiveBucketList::new();
        let header_hash = crate::compute_header_hash(&header).expect("hash");

        manager
            .initialize(
                bucket_list,
                hot_archive_bucket_list,
                header.clone(),
                header_hash,
            )
            .expect("initialization should succeed");

        // Set version below min supported
        manager.set_header_version_for_test(MIN_LEDGER_PROTOCOL_VERSION - 1);

        let close_data = LedgerCloseData::new(
            2,
            TransactionSetVariant::Classic(TransactionSet {
                previous_ledger_hash: header_hash.into(),
                txs: VecM::default(),
            }),
            1,
            header_hash,
        );
        // This should panic
        let _result = manager.begin_close(close_data);
    }

    #[test]
    fn test_ledger_manager_config_default() {
        let config = LedgerManagerConfig::default();
        assert!(config.validate_bucket_hash);
        assert_eq!(config.bucket_list_db.memory_for_caching_mb, 1024);
    }

    #[test]
    fn test_bucket_list_db_config_applied_on_initialize() {
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            bucket_list_db: BucketListDbConfig {
                memory_for_caching_mb: 256,
                index_page_size_exponent: 16,
                ..Default::default()
            },
            ..Default::default()
        };
        let manager = LedgerManager::new(
            "Test SDF Network ; September 2015".to_string(),
            config,
        );

        // Before initialize, bucket list has no config
        assert!(manager.bucket_list.read().bucket_list_db_config().is_none());

        // Initialize with empty bucket lists
        let header = create_genesis_header();
        let header_hash = Hash256::ZERO;
        manager
            .initialize(
                BucketList::new(),
                HotArchiveBucketList::new(),
                header,
                header_hash,
            )
            .unwrap();

        // After initialize, config should be applied
        let bl = manager.bucket_list.read();
        let applied = bl.bucket_list_db_config().expect("config should be set");
        assert_eq!(applied.memory_for_caching_mb, 256);
        assert_eq!(applied.index_page_size_exponent, 16);
    }

    #[test]
    fn test_bucket_list_db_config_survives_reset_and_reinitialize() {
        let config = LedgerManagerConfig {
            validate_bucket_hash: false,
            bucket_list_db: BucketListDbConfig {
                memory_for_caching_mb: 128,
                ..Default::default()
            },
            ..Default::default()
        };
        let manager = LedgerManager::new(
            "Test SDF Network ; September 2015".to_string(),
            config,
        );

        // Initialize
        manager
            .initialize(
                BucketList::new(),
                HotArchiveBucketList::new(),
                create_genesis_header(),
                Hash256::ZERO,
            )
            .unwrap();

        // Verify config is set
        assert_eq!(
            manager.bucket_list.read().bucket_list_db_config().unwrap().memory_for_caching_mb,
            128
        );

        // Reset clears everything
        manager.reset();
        assert!(manager.bucket_list.read().bucket_list_db_config().is_none());

        // Re-initialize should re-apply config
        manager
            .initialize(
                BucketList::new(),
                HotArchiveBucketList::new(),
                create_genesis_header(),
                Hash256::ZERO,
            )
            .unwrap();

        assert_eq!(
            manager.bucket_list.read().bucket_list_db_config().unwrap().memory_for_caching_mb,
            128
        );
    }

    #[test]
    fn test_ledger_close_meta_includes_scp_history() {
        let scp_entry = ScpHistoryEntry::V0(ScpHistoryEntryV0 {
            quorum_sets: VecM::default(),
            ledger_messages: LedgerScpMessages {
                ledger_seq: 1,
                messages: VecM::default(),
            },
        });
        let close_data = LedgerCloseData::new(
            1,
            TransactionSetVariant::Classic(TransactionSet {
                previous_ledger_hash: Hash::from(Hash256::ZERO),
                txs: VecM::default(),
            }),
            0,
            Hash256::ZERO,
        )
        .with_scp_history(vec![scp_entry.clone()]);

        let header = create_genesis_header();
        let meta = build_ledger_close_meta(close_data.tx_set, close_data.scp_history, &header, Hash256::ZERO, Vec::new(), Vec::new(), 0, Vec::new(), false, 0);
        let scp_info_len = match meta {
            LedgerCloseMeta::V0(_) => 0,
            LedgerCloseMeta::V1(v1) => v1.scp_info.len(),
            LedgerCloseMeta::V2(v2) => v2.scp_info.len(),
        };
        assert_eq!(scp_info_len, 1);
    }

    #[test]
    fn test_pending_eviction_scan_initialized_as_none() {
        let manager = LedgerManager::new(
            "Test SDF Network ; September 2015".to_string(),
            LedgerManagerConfig::default(),
        );
        assert!(
            manager.pending_eviction_scan.lock().is_none(),
            "pending_eviction_scan should be None on creation"
        );
    }

    #[test]
    fn test_pending_eviction_scan_cleared_on_reset() {
        let manager = LedgerManager::new(
            "Test SDF Network ; September 2015".to_string(),
            LedgerManagerConfig::default(),
        );

        // Simulate storing a pending scan
        let snapshot = BucketListSnapshot::new(
            &BucketList::default(),
            create_genesis_header(),
        );
        let settings = StateArchivalSettings::default();
        let iter = EvictionIterator::new(settings.starting_eviction_scan_level);
        let settings_clone = settings.clone();
        let handle = std::thread::spawn(move || {
            snapshot.scan_for_eviction_incremental(iter, 2, &settings_clone)
        });
        *manager.pending_eviction_scan.lock() = Some(PendingEvictionScan {
            handle,
            target_ledger_seq: 2,
            settings,
        });

        assert!(manager.pending_eviction_scan.lock().is_some());

        // Reset should clear the pending scan
        manager.reset();

        assert!(
            manager.pending_eviction_scan.lock().is_none(),
            "pending_eviction_scan should be None after reset"
        );
    }

    #[test]
    fn test_pending_eviction_scan_thread_completes() {
        // Verify that a background eviction scan thread can complete and
        // its result can be joined successfully.
        let mut bl = BucketList::new();

        // Add some Soroban entries with TTLs
        let mut entries = Vec::new();
        for i in 0..3u8 {
            let mut hash_bytes = [0u8; 32];
            hash_bytes[0] = i;
            let code_entry = LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::ContractCode(
                    stellar_xdr::curr::ContractCodeEntry {
                        ext: stellar_xdr::curr::ContractCodeEntryExt::V0,
                        hash: Hash(hash_bytes),
                        code: vec![0u8; 50].try_into().unwrap(),
                    },
                ),
                ext: LedgerEntryExt::V0,
            };
            let code_key = LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
                hash: Hash(hash_bytes),
            });

            // Create TTL entry with SHA256 hash of the key
            use sha2::{Digest, Sha256};
            let key_bytes = code_key.to_xdr(stellar_xdr::curr::Limits::none()).unwrap();
            let mut hasher = Sha256::new();
            hasher.update(&key_bytes);
            let hash_result = hasher.finalize();
            let mut key_hash = [0u8; 32];
            key_hash.copy_from_slice(&hash_result);

            let ttl_entry = LedgerEntry {
                last_modified_ledger_seq: 1,
                data: LedgerEntryData::Ttl(TtlEntry {
                    key_hash: Hash(key_hash),
                    live_until_ledger_seq: 3, // Expires at ledger 3
                }),
                ext: LedgerEntryExt::V0,
            };

            entries.push(code_entry);
            entries.push(ttl_entry);
        }

        bl.add_batch(1, TEST_PROTOCOL, BucketListType::Live, entries, vec![], vec![])
            .unwrap();

        let snapshot = BucketListSnapshot::new(&bl, create_genesis_header());
        let settings = StateArchivalSettings {
            starting_eviction_scan_level: 0,
            eviction_scan_size: 100_000,
            max_entries_to_archive: 1000,
            ..Default::default()
        };
        let iter = EvictionIterator {
            bucket_list_level: 0,
            is_curr_bucket: true,
            bucket_file_offset: 0,
        };

        let handle = std::thread::spawn(move || {
            snapshot.scan_for_eviction_incremental(iter, 5, &settings)
        });

        let result = handle.join().expect("thread should not panic").unwrap();
        assert_eq!(result.candidates.len(), 3, "Should find 3 expired entries");
        assert!(result.bytes_scanned > 0);
    }

    // ---- Genesis createLedgerEntries tests ----

    /// Helper to create a minimal `LedgerCloseContext` for testing genesis entry creation.
    ///
    /// The returned context has an empty snapshot and delta at the given ledger_seq.
    /// The manager is initialized with an empty bucket list.
    fn make_test_close_context(
        manager: &LedgerManager,
        ledger_seq: u32,
    ) -> LedgerCloseContext<'_> {
        let header = create_genesis_header();
        let header_hash = crate::compute_header_hash(&header).expect("hash");
        let snapshot = SnapshotHandle::new(crate::snapshot::LedgerSnapshot::empty(0));

        LedgerCloseContext {
            manager,
            close_data: LedgerCloseData::new(
                ledger_seq,
                TransactionSetVariant::Classic(TransactionSet {
                    previous_ledger_hash: header_hash.into(),
                    txs: VecM::default(),
                }),
                1,
                header_hash,
            ),
            prev_header: header.clone(),
            prev_header_hash: header_hash,
            delta: LedgerDelta::new(ledger_seq),
            snapshot,
            stats: LedgerCloseStats::new(),
            upgrade_ctx: UpgradeContext::new(0),
            id_pool: 0,
            tx_results: Vec::new(),
            tx_result_metas: Vec::new(),
            hot_archive_restored_keys: Vec::new(),
            runtime_handle: None,
            start: std::time::Instant::now(),
            timing_begin_close_us: 0,
            timing_tx_exec_us: 0,
            timing_classic_exec_us: 0,
            timing_soroban_exec_us: 0,
            timing_prepare_us: 0,
            timing_config_load_us: 0,
            timing_executor_setup_us: 0,
            timing_fee_pre_deduct_us: 0,
            timing_post_exec_us: 0,
            tx_perf: Vec::new(),
            soroban_fee_write_1kb: 0,
        }
    }

    /// Helper to extract a ConfigSettingEntry from the delta by ConfigSettingId.
    fn get_config_setting_from_delta(
        delta: &LedgerDelta,
        id: ConfigSettingId,
    ) -> Option<ConfigSettingEntry> {
        let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: id,
        });
        delta.get_change(&key).and_then(|change| {
            change.current_entry().and_then(|entry| {
                if let LedgerEntryData::ConfigSetting(ref cs) = entry.data {
                    Some(cs.clone())
                } else {
                    None
                }
            })
        })
    }

    #[test]
    fn test_create_ledger_entries_for_v20_creates_14_entries() {
        let manager = LedgerManager::new(
            "Test SDF Network ; September 2015".to_string(),
            LedgerManagerConfig {
                validate_bucket_hash: false,
                ..Default::default()
            },
        );

        // Initialize with an empty bucket list
        let bucket_list = henyey_bucket::BucketList::new();
        let hot_archive_bucket_list = henyey_bucket::HotArchiveBucketList::new();
        let header = create_genesis_header();
        let header_hash = crate::compute_header_hash(&header).expect("hash");
        manager
            .initialize(bucket_list, hot_archive_bucket_list, header, header_hash)
            .expect("init");

        let mut ctx = make_test_close_context(&manager, 2);
        ctx.create_ledger_entries_for_v20()
            .expect("V20 entries should be created");

        // Verify all 14 entries were created
        // 1. ContractMaxSizeBytes
        let entry =
            get_config_setting_from_delta(&ctx.delta, ConfigSettingId::ContractMaxSizeBytes);
        assert!(entry.is_some(), "ContractMaxSizeBytes should exist");
        if let Some(ConfigSettingEntry::ContractMaxSizeBytes(v)) = entry {
            assert_eq!(v, 2_000);
        } else {
            panic!("Wrong type for ContractMaxSizeBytes");
        }

        // 2. ContractDataKeySizeBytes
        let entry = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::ContractDataKeySizeBytes,
        );
        assert!(entry.is_some(), "ContractDataKeySizeBytes should exist");

        // 3. ContractDataEntrySizeBytes
        let entry = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::ContractDataEntrySizeBytes,
        );
        assert!(entry.is_some(), "ContractDataEntrySizeBytes should exist");

        // 4. ContractComputeV0
        let entry =
            get_config_setting_from_delta(&ctx.delta, ConfigSettingId::ContractComputeV0);
        assert!(entry.is_some(), "ContractComputeV0 should exist");
        if let Some(ConfigSettingEntry::ContractComputeV0(ref compute)) = entry {
            assert_eq!(compute.tx_max_instructions, 2_500_000);
            assert_eq!(compute.tx_memory_limit, 2_000_000);
        }

        // 5. ContractLedgerCostV0
        let entry =
            get_config_setting_from_delta(&ctx.delta, ConfigSettingId::ContractLedgerCostV0);
        assert!(entry.is_some(), "ContractLedgerCostV0 should exist");
        if let Some(ConfigSettingEntry::ContractLedgerCostV0(ref cost)) = entry {
            assert_eq!(cost.tx_max_disk_read_entries, 3);
            assert_eq!(
                cost.soroban_state_target_size_bytes,
                30 * 1024 * 1024 * 1024_i64
            );
        }

        // 6-9. Historical data, events, bandwidth, execution lanes
        assert!(
            get_config_setting_from_delta(
                &ctx.delta,
                ConfigSettingId::ContractHistoricalDataV0
            )
            .is_some(),
            "ContractHistoricalDataV0 should exist"
        );
        assert!(
            get_config_setting_from_delta(&ctx.delta, ConfigSettingId::ContractEventsV0)
                .is_some(),
            "ContractEventsV0 should exist"
        );
        assert!(
            get_config_setting_from_delta(&ctx.delta, ConfigSettingId::ContractBandwidthV0)
                .is_some(),
            "ContractBandwidthV0 should exist"
        );
        assert!(
            get_config_setting_from_delta(
                &ctx.delta,
                ConfigSettingId::ContractExecutionLanes
            )
            .is_some(),
            "ContractExecutionLanes should exist"
        );

        // 10-11. CPU and memory cost params (23 entries each)
        let cpu = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::ContractCostParamsCpuInstructions,
        );
        assert!(cpu.is_some(), "CPU cost params should exist");
        if let Some(ConfigSettingEntry::ContractCostParamsCpuInstructions(ref params)) = cpu {
            assert_eq!(
                params.0.len(),
                23,
                "V20 CPU cost params should have 23 entries"
            );
        }

        let mem = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::ContractCostParamsMemoryBytes,
        );
        assert!(mem.is_some(), "Memory cost params should exist");
        if let Some(ConfigSettingEntry::ContractCostParamsMemoryBytes(ref params)) = mem {
            assert_eq!(
                params.0.len(),
                23,
                "V20 memory cost params should have 23 entries"
            );
        }

        // 12. StateArchival
        let archival =
            get_config_setting_from_delta(&ctx.delta, ConfigSettingId::StateArchival);
        assert!(archival.is_some(), "StateArchival should exist");
        if let Some(ConfigSettingEntry::StateArchival(ref sa)) = archival {
            assert_eq!(sa.max_entry_ttl, 1_054_080);
            assert_eq!(sa.min_temporary_ttl, 16);
            assert_eq!(sa.min_persistent_ttl, 4_096);
            assert_eq!(sa.starting_eviction_scan_level, 6);
        }

        // 13. LiveSorobanStateSizeWindow (30-entry window)
        let window = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::LiveSorobanStateSizeWindow,
        );
        assert!(window.is_some(), "LiveSorobanStateSizeWindow should exist");
        if let Some(ConfigSettingEntry::LiveSorobanStateSizeWindow(ref w)) = window {
            assert_eq!(w.len(), 30, "Window should have 30 entries");
        }

        // 14. EvictionIterator
        let eviction =
            get_config_setting_from_delta(&ctx.delta, ConfigSettingId::EvictionIterator);
        assert!(eviction.is_some(), "EvictionIterator should exist");
        if let Some(ConfigSettingEntry::EvictionIterator(ref ei)) = eviction {
            assert_eq!(ei.bucket_list_level, 6);
            assert!(ei.is_curr_bucket);
            assert_eq!(ei.bucket_file_offset, 0);
        }
    }

    #[test]
    fn test_create_cost_types_for_v21_resizes_to_45() {
        let manager = LedgerManager::new(
            "Test SDF Network ; September 2015".to_string(),
            LedgerManagerConfig {
                validate_bucket_hash: false,
                ..Default::default()
            },
        );
        let bucket_list = henyey_bucket::BucketList::new();
        let hot_archive_bucket_list = henyey_bucket::HotArchiveBucketList::new();
        let header = create_genesis_header();
        let header_hash = crate::compute_header_hash(&header).expect("hash");
        manager
            .initialize(bucket_list, hot_archive_bucket_list, header, header_hash)
            .expect("init");

        let mut ctx = make_test_close_context(&manager, 2);

        // First create V20 entries (prerequisite)
        ctx.create_ledger_entries_for_v20()
            .expect("V20 entries should be created");

        // Now apply V21 upgrade
        ctx.create_cost_types_for_v21()
            .expect("V21 cost types should be created");

        // CPU params should now be 45 entries
        let cpu = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::ContractCostParamsCpuInstructions,
        );
        if let Some(ConfigSettingEntry::ContractCostParamsCpuInstructions(ref params)) = cpu {
            assert_eq!(
                params.0.len(),
                45,
                "V21 CPU cost params should have 45 entries"
            );
            // Check VmCachedInstantiation was updated (index 12)
            assert_eq!(params.0[12].const_term, 41142);
            assert_eq!(params.0[12].linear_term, 634);
            // Check new ParseWasmInstructions entry (index 23)
            assert_eq!(params.0[23].const_term, 73077);
            // Check last entry VerifyEcdsaSecp256r1Sig (index 44)
            assert_eq!(params.0[44].const_term, 3000906);
        } else {
            panic!("CPU cost params not found or wrong type");
        }

        // Memory params should now be 45 entries
        let mem = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::ContractCostParamsMemoryBytes,
        );
        if let Some(ConfigSettingEntry::ContractCostParamsMemoryBytes(ref params)) = mem {
            assert_eq!(
                params.0.len(),
                45,
                "V21 memory cost params should have 45 entries"
            );
            // Check VmCachedInstantiation was updated (index 12)
            assert_eq!(params.0[12].const_term, 69472);
        } else {
            panic!("Memory cost params not found or wrong type");
        }
    }

    #[test]
    fn test_create_cost_types_for_v22_resizes_to_70() {
        let manager = LedgerManager::new(
            "Test SDF Network ; September 2015".to_string(),
            LedgerManagerConfig {
                validate_bucket_hash: false,
                ..Default::default()
            },
        );
        let bucket_list = henyey_bucket::BucketList::new();
        let hot_archive_bucket_list = henyey_bucket::HotArchiveBucketList::new();
        let header = create_genesis_header();
        let header_hash = crate::compute_header_hash(&header).expect("hash");
        manager
            .initialize(bucket_list, hot_archive_bucket_list, header, header_hash)
            .expect("init");

        let mut ctx = make_test_close_context(&manager, 2);

        // Chain V20 -> V21 -> V22
        ctx.create_ledger_entries_for_v20().expect("V20");
        ctx.create_cost_types_for_v21().expect("V21");
        ctx.create_cost_types_for_v22().expect("V22");

        // CPU params should now be 70 entries
        let cpu = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::ContractCostParamsCpuInstructions,
        );
        if let Some(ConfigSettingEntry::ContractCostParamsCpuInstructions(ref params)) = cpu {
            assert_eq!(
                params.0.len(),
                70,
                "V22 CPU cost params should have 70 entries"
            );
            // Check first BLS12-381 entry (index 45): Bls12381EncodeFp
            assert_eq!(params.0[45].const_term, 661);
            // Check last BLS12-381 entry (index 69): Bls12381FrInv
            assert_eq!(params.0[69].const_term, 35421);
        } else {
            panic!("CPU cost params not found or wrong type");
        }

        // Memory params should now be 70 entries
        let mem = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::ContractCostParamsMemoryBytes,
        );
        if let Some(ConfigSettingEntry::ContractCostParamsMemoryBytes(ref params)) = mem {
            assert_eq!(
                params.0.len(),
                70,
                "V22 memory cost params should have 70 entries"
            );
            // Check Bls12381G1Msm memory (index 55) has non-zero values
            assert_eq!(params.0[55].const_term, 109494);
            assert_eq!(params.0[55].linear_term, 354667);
        } else {
            panic!("Memory cost params not found or wrong type");
        }
    }

    #[test]
    fn test_create_and_update_ledger_entries_for_v23() {
        let manager = LedgerManager::new(
            "Test SDF Network ; September 2015".to_string(),
            LedgerManagerConfig {
                validate_bucket_hash: false,
                ..Default::default()
            },
        );
        let bucket_list = henyey_bucket::BucketList::new();
        let hot_archive_bucket_list = henyey_bucket::HotArchiveBucketList::new();
        let header = create_genesis_header();
        let header_hash = crate::compute_header_hash(&header).expect("hash");
        manager
            .initialize(bucket_list, hot_archive_bucket_list, header, header_hash)
            .expect("init");

        let mut ctx = make_test_close_context(&manager, 2);

        // Chain V20 -> V21 -> V22 -> V23
        ctx.create_ledger_entries_for_v20().expect("V20");
        ctx.create_cost_types_for_v21().expect("V21");
        ctx.create_cost_types_for_v22().expect("V22");
        ctx.create_and_update_ledger_entries_for_v23().expect("V23");

        // 1. ContractParallelComputeV0
        let parallel = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::ContractParallelComputeV0,
        );
        assert!(
            parallel.is_some(),
            "ContractParallelComputeV0 should exist"
        );
        if let Some(ConfigSettingEntry::ContractParallelComputeV0(ref p)) = parallel {
            assert_eq!(p.ledger_max_dependent_tx_clusters, 1);
        }

        // 2. ScpTiming
        let timing =
            get_config_setting_from_delta(&ctx.delta, ConfigSettingId::ScpTiming);
        assert!(timing.is_some(), "ScpTiming should exist");
        if let Some(ConfigSettingEntry::ScpTiming(ref t)) = timing {
            assert_eq!(t.ledger_target_close_time_milliseconds, 5000);
            assert_eq!(t.nomination_timeout_initial_milliseconds, 1000);
        }

        // 3. ContractLedgerCostExtV0
        let ext = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::ContractLedgerCostExtV0,
        );
        assert!(ext.is_some(), "ContractLedgerCostExtV0 should exist");
        if let Some(ConfigSettingEntry::ContractLedgerCostExtV0(ref e)) = ext {
            // tx_max_footprint_entries should match the V0 tx_max_disk_read_entries
            assert_eq!(e.tx_max_footprint_entries, 3);
            assert_eq!(e.fee_write1_kb, 3_500);
        }

        // 4. Verify rent cost params were updated
        let cost = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::ContractLedgerCostV0,
        );
        if let Some(ConfigSettingEntry::ContractLedgerCostV0(ref c)) = cost {
            assert_eq!(c.soroban_state_target_size_bytes, 3_000_000_000);
            assert_eq!(c.rent_fee1_kb_soroban_state_size_low, -17_000);
            assert_eq!(c.rent_fee1_kb_soroban_state_size_high, 10_000);
        } else {
            panic!("ContractLedgerCostV0 not found or wrong type after V23 upgrade");
        }

        let archival =
            get_config_setting_from_delta(&ctx.delta, ConfigSettingId::StateArchival);
        if let Some(ConfigSettingEntry::StateArchival(ref sa)) = archival {
            assert_eq!(sa.persistent_rent_rate_denominator, 1_215);
            assert_eq!(sa.temp_rent_rate_denominator, 2_430);
        } else {
            panic!("StateArchival not found or wrong type after V23 upgrade");
        }
    }

    #[test]
    fn test_full_chain_v20_through_v25_cost_params() {
        let manager = LedgerManager::new(
            "Test SDF Network ; September 2015".to_string(),
            LedgerManagerConfig {
                validate_bucket_hash: false,
                ..Default::default()
            },
        );
        let bucket_list = henyey_bucket::BucketList::new();
        let hot_archive_bucket_list = henyey_bucket::HotArchiveBucketList::new();
        let header = create_genesis_header();
        let header_hash = crate::compute_header_hash(&header).expect("hash");
        manager
            .initialize(bucket_list, hot_archive_bucket_list, header, header_hash)
            .expect("init");

        let mut ctx = make_test_close_context(&manager, 2);

        // Full chain: V20 -> V21 -> V22 -> V23 -> V25
        ctx.create_ledger_entries_for_v20().expect("V20");
        ctx.create_cost_types_for_v21().expect("V21");
        ctx.create_cost_types_for_v22().expect("V22");
        ctx.create_and_update_ledger_entries_for_v23().expect("V23");
        ctx.create_cost_types_for_v25().expect("V25");

        // CPU cost params should have 85 entries (BN254)
        let cpu = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::ContractCostParamsCpuInstructions,
        );
        if let Some(ConfigSettingEntry::ContractCostParamsCpuInstructions(ref params)) = cpu {
            assert_eq!(
                params.0.len(),
                85,
                "V25 CPU cost params should have 85 entries"
            );
            // Verify original V20 entry preserved (index 0: WasmInsnExec)
            assert_eq!(params.0[0].const_term, 4);
            // Verify V21 entry preserved (index 23: ParseWasmInstructions)
            assert_eq!(params.0[23].const_term, 73077);
            // Verify V22 entry preserved (index 45: Bls12381EncodeFp)
            assert_eq!(params.0[45].const_term, 661);
            // Verify V25 BN254 entry (index 70: Bn254EncodeFp)
            assert_eq!(params.0[70].const_term, 344);
            // Verify last V25 entry (index 84: Bn254FrInv)
            assert_eq!(params.0[84].const_term, 33151);
        } else {
            panic!("CPU cost params not found after full chain");
        }

        // Memory cost params should also have 85 entries
        let mem = get_config_setting_from_delta(
            &ctx.delta,
            ConfigSettingId::ContractCostParamsMemoryBytes,
        );
        if let Some(ConfigSettingEntry::ContractCostParamsMemoryBytes(ref params)) = mem {
            assert_eq!(
                params.0.len(),
                85,
                "V25 memory cost params should have 85 entries"
            );
        } else {
            panic!("Memory cost params not found after full chain");
        }
    }

    #[test]
    fn test_v20_cpu_cost_params_values_match_stellar_core() {
        // Parity: NetworkConfig.cpp:246-338 initialCpuCostParamsEntryForV20
        let cpu_params = LedgerCloseContext::initial_cpu_cost_params_for_v20();
        assert_eq!(cpu_params.len(), 23);

        // Spot-check key values against stellar-core
        assert_eq!(cpu_params[0].const_term, 4); // WasmInsnExec
        assert_eq!(cpu_params[0].linear_term, 0);
        assert_eq!(cpu_params[7].const_term, 59052); // ValDeser
        assert_eq!(cpu_params[7].linear_term, 4001);
        assert_eq!(cpu_params[10].const_term, 377524); // VerifyEd25519Sig
        assert_eq!(cpu_params[11].const_term, 451626); // VmInstantiation
        assert_eq!(cpu_params[12].const_term, 451626); // VmCachedInstantiation (same as VmInstantiation in V20)
        assert_eq!(cpu_params[16].const_term, 2315295); // RecoverEcdsaSecp256k1Key
        assert_eq!(cpu_params[22].const_term, 1058); // ChaCha20DrawBytes
        assert_eq!(cpu_params[22].linear_term, 501);
    }

    #[test]
    fn test_v20_mem_cost_params_values_match_stellar_core() {
        // Parity: NetworkConfig.cpp:688-776 initialMemCostParamsEntryForV20
        let mem_params = LedgerCloseContext::initial_mem_cost_params_for_v20();
        assert_eq!(mem_params.len(), 23);

        // Spot-check key values
        assert_eq!(mem_params[0].const_term, 0); // WasmInsnExec (no memory)
        assert_eq!(mem_params[1].const_term, 16); // MemAlloc
        assert_eq!(mem_params[1].linear_term, 128);
        assert_eq!(mem_params[11].const_term, 130065); // VmInstantiation
        assert_eq!(mem_params[11].linear_term, 5064);
        assert_eq!(mem_params[16].const_term, 181); // RecoverEcdsaSecp256k1Key
    }
}
