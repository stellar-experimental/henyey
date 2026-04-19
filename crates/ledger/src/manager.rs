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

use crate::offer_store::OfferStore;
use crate::{
    close::{
        LedgerCloseData, LedgerCloseResult, LedgerCloseStats, SortState, TransactionSetVariant,
        UpgradeContext,
    },
    close_state::CloseLedgerState,
    delta::EntryChange,
    execution::{
        execute_soroban_parallel_phase, load_soroban_network_info, pre_deduct_all_fees_on_delta,
        run_transactions_on_executor, LedgerAdvanceParams, SorobanContext, SorobanNetworkInfo,
        TransactionExecutionResult, TransactionExecutor, TxSetResult,
    },
    header::{compute_header_hash, create_next_header, NextHeaderFields},
    snapshot::{LedgerSnapshot, SnapshotHandle},
    LedgerError, Result,
};
use henyey_bucket::{
    BucketEntry, BucketEntryExt, BucketList, BucketListSnapshot, BucketMergeMap, EvictionIterator,
    EvictionIteratorExt, EvictionResult, HotArchiveBucketList,
};
use henyey_common::protocol::{
    hot_archive_supported, needs_upgrade_to_version, protocol_version_starts_from, ProtocolVersion,
};
use henyey_common::{BucketListDbConfig, Hash256, NetworkId};
use henyey_tx::soroban::PersistentModuleCache;
use henyey_tx::{ClassicEventConfig, LedgerContext, TxEventManager};
use parking_lot::{Mutex, RwLock};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use stellar_xdr::curr::{
    AccountId, BucketListType, ConfigSettingEntry, ConfigSettingId, ExtensionPoint,
    GeneralizedTransactionSet, Hash, LedgerCloseMeta, LedgerCloseMetaExt, LedgerCloseMetaExtV1,
    LedgerCloseMetaV2, LedgerEntry, LedgerEntryData, LedgerEntryExt, LedgerHeader,
    LedgerHeaderHistoryEntry, LedgerHeaderHistoryEntryExt, LedgerKey, LedgerKeyConfigSetting,
    PoolId, ScpHistoryEntry, StateArchivalSettings, TransactionEventStage, TransactionMeta,
    TransactionPhase, TransactionResultMetaV1, TransactionSet, TransactionSetV1, TxSetComponent,
    TxSetComponentTxsMaybeDiscountedFee, UpgradeEntryMeta, VecM,
};
use tracing::{debug, info};

/// Standard Linux memory page size in bytes.
#[allow(dead_code)]
const PAGE_SIZE: u64 = 4096;

/// Heuristic multiplier for estimating compiled module size from WASM byte size.
/// Compiled modules are approximately 4× their raw WASM size.
const WASM_COMPILED_SIZE_MULTIPLIER: u64 = 4;

/// Read current RSS (Resident Set Size) in bytes from /proc/self/statm.
/// Returns 0 on non-Linux or on error.
fn get_rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
            // Format: size resident shared text lib data dt (all in pages)
            let page_size = PAGE_SIZE;
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
    ttl_entries: HashMap<
        Hash,
        (
            stellar_xdr::curr::LedgerKeyTtl,
            crate::soroban_state::TtlData,
        ),
    >,
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
    ttl_entries: HashMap<
        Hash,
        (
            stellar_xdr::curr::LedgerKeyTtl,
            crate::soroban_state::TtlData,
        ),
    >,
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
                self.ttl_entries
                    .insert(ttl.key_hash.clone(), (ttl_key, ttl_data));
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
) -> Result<LevelScanResult> {
    let mut scanner = LevelScanner::new();

    // Scan curr first, then snap (curr shadows snap within a level)
    for bucket in [curr, snap] {
        for entry_result in bucket.iter()? {
            let entry = entry_result?;
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

            scanner.process_entry(&entry, key, soroban_enabled, module_cache, protocol_version);
        }
    }

    Ok(scanner.into_result())
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

/// Streaming scan-and-merge: fuses the scan and merge phases into a single pass.
///
/// Instead of building an intermediate `LevelScanResult` per level and then merging it,
/// this function processes each bucket entry inline and merges it directly into the
/// final accumulators (soroban state, offer map, etc.) as it's read from the bucket.
///
/// This eliminates the double-buffering that occurs when a large `LevelScanResult`
/// (e.g. level 10 with ~12M entries) coexists in memory alongside the growing final state
/// during the merge transition.
///
/// Cross-level dedup uses the same `global_seen` / `global_ttl_seen` sets as the
/// parallel path. Intra-level dedup uses a per-level `seen_keys` set that is reset
/// between levels. Levels are processed in order (0 → 10) so that lower-numbered
/// levels (newer data) correctly shadow higher-numbered levels.
///
/// This path is used when `scan_thread_count == 1`.
fn scan_and_merge_streaming(
    level_pairs: &[(Arc<henyey_bucket::Bucket>, Arc<henyey_bucket::Bucket>)],
    protocol_version: u32,
    soroban_enabled: bool,
    rent_config: &Option<crate::soroban_state::SorobanRentConfig>,
    module_cache: Option<PersistentModuleCache>,
) -> Result<CacheInitResult> {
    let module_cache_arc = module_cache.map(Arc::new);

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

    for (level_idx, (curr, snap)) in level_pairs.iter().enumerate() {
        let level_start = std::time::Instant::now();
        let mut seen_keys: HashSet<LedgerKey> = HashSet::new();
        let mut level_entry_count = 0u64;
        let mut level_ttl_count = 0u64;

        // Scan curr first, then snap (curr shadows snap within a level)
        for bucket in [curr.as_ref(), snap.as_ref()] {
            for entry_result in bucket.iter()? {
                let entry = entry_result?;
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

                // Intra-level dedup: curr shadows snap
                if seen_keys.contains(&key) {
                    continue;
                }

                // Skip soroban types if not enabled
                if !soroban_enabled && !matches!(&key, LedgerKey::Offer(_)) {
                    continue;
                }

                seen_keys.insert(key.clone());

                if let BucketEntry::Liveentry(ref le) | BucketEntry::Initentry(ref le) = entry {
                    // Compile contracts via the shared module cache
                    if let LedgerEntryData::ContractCode(ref contract_code) = le.data {
                        if let Some(ref cache) = module_cache_arc {
                            cache.add_contract(contract_code.code.as_slice(), protocol_version);
                        }
                    }

                    // TTL entries: cross-level dedup and merge inline
                    if let LedgerEntryData::Ttl(ref ttl) = le.data {
                        let key_hash = ttl.key_hash.clone();
                        if global_ttl_seen.insert(key_hash.clone()) {
                            let ttl_key = stellar_xdr::curr::LedgerKeyTtl {
                                key_hash: key_hash.clone(),
                            };
                            let ttl_data = crate::soroban_state::TtlData::new(
                                ttl.live_until_ledger_seq,
                                le.last_modified_ledger_seq,
                            );
                            if let Err(e) = soroban_state.create_ttl(&ttl_key, ttl_data) {
                                tracing::trace!(error = %e, "Failed to add TTL to soroban state (may be pending)");
                            } else {
                                ttl_count += 1;
                                level_ttl_count += 1;
                            }
                        }
                    } else {
                        // Non-TTL live entry: cross-level dedup and merge inline
                        if !global_seen.insert(key.clone()) {
                            continue;
                        }
                        match &le.data {
                            LedgerEntryData::Offer(ref offer) => {
                                let offer_id = offer.offer_id;
                                mem_offers.insert(offer_id, le.clone());
                                offer_count += 1;
                                level_entry_count += 1;
                            }
                            LedgerEntryData::Trustline(ref tl) => {
                                if let stellar_xdr::curr::TrustLineAsset::PoolShare(ref pool_id) =
                                    tl.asset
                                {
                                    pool_share_tl_account_index
                                        .entry(tl.account_id.clone())
                                        .or_default()
                                        .insert(pool_id.clone());
                                }
                                level_entry_count += 1;
                            }
                            LedgerEntryData::ContractCode(_) => {
                                if let Err(e) = soroban_state.create_contract_code(
                                    le.clone(),
                                    protocol_version,
                                    rent_config.as_ref(),
                                ) {
                                    tracing::warn!(error = %e, "Failed to add contract code to soroban state");
                                } else {
                                    code_count += 1;
                                    level_entry_count += 1;
                                }
                            }
                            LedgerEntryData::ContractData(_) => {
                                if let Err(e) = soroban_state.create_contract_data(le.clone()) {
                                    tracing::warn!(error = %e, "Failed to add contract data to soroban state");
                                } else {
                                    data_count += 1;
                                    level_entry_count += 1;
                                }
                            }
                            LedgerEntryData::ConfigSetting(_) => {
                                if let Err(e) = soroban_state.process_entry_create(
                                    le,
                                    protocol_version,
                                    rent_config.as_ref(),
                                ) {
                                    tracing::warn!(error = %e, "Failed to add config setting to soroban state");
                                } else {
                                    config_count += 1;
                                    level_entry_count += 1;
                                }
                            }
                            _ => {}
                        }
                    }
                } else if let BucketEntry::Deadentry(_) = entry {
                    // Dead keys shadow live entries at higher (older) levels.
                    if let LedgerKey::Ttl(ref ttl_key) = key {
                        global_ttl_seen.insert(ttl_key.key_hash.clone());
                    }
                    global_seen.insert(key);
                }
            }
        }

        // Per-level seen_keys is dropped here, freeing intra-level dedup memory
        // before scanning the next level.
        info!(
            level = level_idx,
            entries = level_entry_count,
            ttls = level_ttl_count,
            elapsed_ms = level_start.elapsed().as_millis() as u64,
            "scan_and_merge_streaming: level scan+merge complete"
        );
    }

    info!(
        offer_count,
        code_count, data_count, ttl_count, config_count, "scan_and_merge_streaming: complete"
    );

    let module_cache = module_cache_arc
        .map(|arc| Arc::try_unwrap(arc).unwrap_or_else(|arc| PersistentModuleCache::clone(&arc)));

    Ok(CacheInitResult {
        offers: mem_offers,
        pool_share_tl_account_index,
        module_cache,
        soroban_state,
    })
}

/// Bounded-parallel scan-and-merge: up to `scan_thread_count` threads scan levels
/// concurrently while the main thread merges completed results in level order (0 → 10).
///
/// Levels are claimed in largest-first order so that the biggest (slowest) levels
/// start immediately on all available workers, minimizing wall-clock time.
///
/// Memory is bounded to N concurrent raw `LevelScanResult`s plus accumulated final state,
/// because the main thread merges and drops each raw HashMap before the window slides forward.
///
/// When `scan_thread_count == 1`, delegates to `scan_and_merge_streaming` which fuses the
/// scan and merge phases to avoid allocating intermediate `LevelScanResult` buffers entirely.
fn scan_and_merge(
    level_pairs: &[(Arc<henyey_bucket::Bucket>, Arc<henyey_bucket::Bucket>)],
    protocol_version: u32,
    soroban_enabled: bool,
    rent_config: &Option<crate::soroban_state::SorobanRentConfig>,
    module_cache: Option<PersistentModuleCache>,
    scan_thread_count: usize,
) -> Result<CacheInitResult> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    let num_levels = level_pairs.len();
    let num_workers = scan_thread_count.min(num_levels).max(1);

    // Single-worker fast path: streaming scan-and-merge to avoid double-buffering.
    // This eliminates the intermediate LevelScanResult allocation, which is critical
    // for memory-constrained environments (e.g. CI with 15GB RAM) where level 10
    // alone can have ~12M entries.
    if num_workers == 1 {
        return scan_and_merge_streaming(
            level_pairs,
            protocol_version,
            soroban_enabled,
            rent_config,
            module_cache,
        );
    }

    let module_cache_arc = module_cache.map(Arc::new);

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

    // Choose scan order based on worker count:
    // - Single worker: scan in level order (0 → 10) so the main thread can merge
    //   and free each result immediately, avoiding buffering all results in memory.
    // - Multiple workers: scan largest levels first for better load balancing,
    //   since the main thread can absorb out-of-order completions.
    let mut sorted_indices: Vec<usize> = (0..num_levels).collect();
    if num_workers > 1 {
        sorted_indices
            .sort_by_key(|&i| std::cmp::Reverse(level_pairs[i].0.len() + level_pairs[i].1.len()));
    }

    // Workers atomically claim slots in the sorted array.
    let next_claim = AtomicUsize::new(0);
    let next_claim_ref = &next_claim;
    let sorted_ref = &sorted_indices[..];
    let (tx, rx) = mpsc::channel::<Result<(usize, LevelScanResult)>>();

    let mut scan_error: Option<LedgerError> = None;

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
                    let result =
                        scan_single_level(curr, snap, soroban_enabled, &mc_clone, protocol_version);
                    match &result {
                        Ok(r) => {
                            info!(
                                level = idx,
                                entries = r.entries.len(),
                                ttls = r.ttl_entries.len(),
                                elapsed_ms = level_start.elapsed().as_millis() as u64,
                                "scan_bucket_list_for_caches: level scan complete"
                            );
                        }
                        Err(e) => {
                            tracing::error!(level = idx, error = %e, "scan_bucket_list_for_caches: level scan failed");
                        }
                    }
                    if worker_tx.send(result.map(|r| (idx, r))).is_err() {
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
            let received = rx.recv().expect("worker thread panicked");
            let (idx, result) = match received {
                Ok(pair) => pair,
                Err(e) => {
                    scan_error = Some(e);
                    return;
                }
            };
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
                            if let stellar_xdr::curr::TrustLineAsset::PoolShare(ref pool_id) =
                                tl.asset
                            {
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

    if let Some(e) = scan_error {
        return Err(e);
    }

    info!(
        offer_count,
        code_count,
        data_count,
        ttl_count,
        config_count,
        "scan_bucket_list_for_caches: merge complete"
    );

    let module_cache = module_cache_arc
        .map(|arc| Arc::try_unwrap(arc).unwrap_or_else(|arc| PersistentModuleCache::clone(&arc)));

    Ok(CacheInitResult {
        offers: mem_offers,
        pool_share_tl_account_index,
        module_cache,
        soroban_state,
    })
}

/// Scan a bucket list and extract all cache data.
///
/// Extracts level pairs from the BucketList, then delegates to
/// `scan_level_pairs_for_caches` which computes the rent config internally.
fn scan_bucket_list_for_caches(
    bucket_list: &BucketList,
    protocol_version: u32,
    scan_thread_count: usize,
) -> Result<CacheInitResult> {
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
        if let LedgerEntryData::ConfigSetting(ConfigSettingEntry::ContractCostParamsMemoryBytes(
            params,
        )) = e.data
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
) -> Result<CacheInitResult> {
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
        scan_thread_count, "scan_bucket_list_for_caches: starting scan..."
    );

    let result = scan_and_merge(
        &level_pairs,
        protocol_version,
        soroban_enabled,
        &rent_config,
        module_cache,
        scan_thread_count,
    )?;

    let scan_elapsed = cache_init_start.elapsed();
    info!(
        elapsed_ms = scan_elapsed.as_millis() as u64,
        "scan_bucket_list_for_caches: complete"
    );

    Ok(result)
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

    /// Pre-computed Soroban network info for the current ledger epoch.
    ///
    /// Populated eagerly in `commit_close()` and `initialize*()`, cleared on
    /// `reset()`. `None` means pre-Soroban protocol or not yet initialized.
    /// Reading this field is O(1) — no snapshot or bucket-list lookup required.
    soroban_network_info: Option<SorobanNetworkInfo>,
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
    /// across ledger closes and calling `advance_to_ledger`,
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

    /// Test-only counter: number of `create_snapshot()` invocations.
    ///
    /// Used by regression tests (e.g., #1759) to assert that batch
    /// validation paths build exactly one snapshot per pass rather than
    /// one per `load_account` call. Behind `test-utils` so production
    /// builds pay nothing.
    #[cfg(any(test, feature = "test-utils"))]
    snapshot_count: std::sync::atomic::AtomicU64,
}

// Compile-time assertion: LedgerManager must be Send + Sync for spawn_blocking.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<LedgerManager> as fn();
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
            hot_archive_bucket_list: Arc::new(RwLock::new(Some(HotArchiveBucketList::new()))),
            network_id,
            state: RwLock::new(LedgerState {
                header: create_genesis_header(),
                header_hash: Hash256::ZERO,
                initialized: false,
                soroban_network_info: None,
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
            #[cfg(any(test, feature = "test-utils"))]
            snapshot_count: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Get the network ID.
    pub fn network_id(&self) -> &NetworkId {
        &self.network_id
    }

    /// Test-only: total number of `create_snapshot()` invocations since
    /// construction. Used by regression tests to assert batch validation
    /// paths build one snapshot per pass instead of one per lookup
    /// (see #1759).
    #[cfg(any(test, feature = "test-utils"))]
    pub fn test_snapshot_count(&self) -> u64 {
        self.snapshot_count
            .load(std::sync::atomic::Ordering::Relaxed)
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
        // Recompute from the current bucket-list state before acquiring the
        // write lock so publication is atomic.
        let info = self.compute_soroban_info();
        let mut state = self.state.write();
        state.header = header;
        state.header_hash = header_hash;
        state.soroban_network_info = info;
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
        self.bucket_list
            .write()
            .resolve_all_pending_merges()
            .expect("bucket merge failure is fatal — cannot continue with corrupt bucket list");
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
        self.verify_and_install_bucket_lists(
            bucket_list,
            hot_archive_bucket_list,
            header,
            header_hash,
        )?;
        self.initialize_all_caches(protocol_version, 0)?;

        // Eagerly compute and cache Soroban network info from the initialized state.
        self.state.write().soroban_network_info = self.compute_soroban_info();

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
        self.verify_and_install_bucket_lists(
            bucket_list,
            hot_archive_bucket_list,
            header,
            header_hash,
        )?;
        crate::memory_report::log_startup_memory("after_verify_install_buckets");

        // Initialize per-bucket caches for all DiskIndex buckets.
        {
            let bucket_list = self.bucket_list.read();
            bucket_list.maybe_initialize_caches();
        }
        crate::memory_report::log_startup_memory("after_bucket_cache_init");

        // Install pre-computed cache data.
        *self.offer_store.lock() = OfferStore::from_bucket_list_entries(cache_data.offers);
        *self.pool_share_tl_account_index.write() = cache_data.pool_share_tl_account_index;
        *self.module_cache.write() = cache_data.module_cache;
        *self.soroban_state.write() = cache_data.soroban_state;
        *self.offers_initialized.write() = true;
        crate::memory_report::log_startup_memory("after_cache_install");

        // Eagerly compute and cache Soroban network info from the initialized state.
        self.state.write().soroban_network_info = self.compute_soroban_info();

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
        // When hot archive is supported, the hash is SHA256(live_hash || hot_archive_hash).
        // For earlier protocols, the hash is just the live bucket list hash.
        let live_hash = bucket_list.hash();
        let computed_hash = if hot_archive_supported(header.ledger_version) {
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
        *self.hot_archive_bucket_list.write() = Some(HotArchiveBucketList::new());

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
        state.soroban_network_info = None;

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
        crate::memory_report::log_startup_memory("before_cache_scan");

        let cache_data = {
            let bucket_list = self.bucket_list.read();
            let cache_data = scan_bucket_list_for_caches(
                &bucket_list,
                protocol_version,
                self.config.scan_thread_count,
            )?;
            crate::memory_report::log_startup_memory("after_cache_scan");

            // Initialize per-bucket caches for all DiskIndex buckets.
            // Uses proportional sizing based on the BucketListDB config.
            bucket_list.maybe_initialize_caches();
            crate::memory_report::log_startup_memory("after_bucket_cache_init");
            cache_data
        };

        *self.offer_store.lock() = OfferStore::from_bucket_list_entries(cache_data.offers);
        *self.pool_share_tl_account_index.write() = cache_data.pool_share_tl_account_index;
        *self.module_cache.write() = cache_data.module_cache;
        *self.soroban_state.write() = cache_data.soroban_state;
        *self.offers_initialized.write() = true;
        crate::memory_report::log_startup_memory("after_cache_install");

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
            match state.header.to_xdr(Limits::none()) {
                Ok(header_xdr) => {
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
                }
                Err(e) => {
                    tracing::error!("Failed to encode header XDR for debugging: {e}");
                }
            }

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

        // Create the root CloseLedgerState for this close cycle.
        let ltx = CloseLedgerState::begin(
            snapshot,
            state.header.clone(),
            state.header_hash,
            expected_seq,
        );

        Ok(LedgerCloseContext {
            manager: self,
            close_data,
            prev_header: state.header.clone(),
            prev_header_hash: state.header_hash,
            ltx,
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
        #[cfg(any(test, feature = "test-utils"))]
        self.snapshot_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        //
        // We also snapshot the Soroban state (frozen clone) rather than capturing a live
        // reference. This ensures lookups see data consistent with the header captured
        // above. Without this, a concurrent commit() could update soroban_state entries
        // before commit_close() publishes the new header, causing the snapshot to mix
        // new Soroban data with the old header.
        let soroban_snapshot = Arc::new(self.soroban_state.read().snapshot());
        let bucket_list_snapshot = Arc::new({
            let bl = self.bucket_list.read();
            henyey_bucket::BucketListSnapshot::new(&bl, state.header.clone())
        });
        let soroban_for_lookup = soroban_snapshot.clone();
        let bls_for_lookup = bucket_list_snapshot.clone();
        let lookup_fn: crate::snapshot::EntryLookupFn = Arc::new(move |key: &LedgerKey| {
            // For Soroban entry types, check in-memory state first (O(1)),
            // then fall back to bucket list if not found.
            if crate::soroban_state::InMemorySorobanState::is_in_memory_type(key) {
                if let Some(entry) = soroban_for_lookup.get(key) {
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
        let soroban_for_batch = soroban_snapshot.clone();
        let bls_for_batch = bucket_list_snapshot.clone();
        let batch_lookup_fn: crate::snapshot::BatchEntryLookupFn =
            Arc::new(move |keys: &[LedgerKey]| {
                let mut result = Vec::new();
                let mut bucket_list_keys = Vec::new();

                // Check soroban state snapshot for soroban types first (O(1))
                for key in keys {
                    if crate::soroban_state::InMemorySorobanState::is_in_memory_type(key) {
                        if let Some(entry) = soroban_for_batch.get(key) {
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
                    .map(|pool_ids| pool_ids.iter().cloned().collect())
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
            // When hot archive is supported, the hash is SHA256(live_hash || hot_archive_hash).
            // For earlier protocols, the hash is just the live bucket list hash.
            let computed = if hot_archive_supported(new_header.ledger_version) {
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
                    LedgerEntryData::Trustline(tl)
                        if matches!(tl.asset, stellar_xdr::curr::TrustLineAsset::PoolShare(_)) =>
                    {
                        match change {
                            EntryChange::Created(entry) => {
                                if let LedgerEntryData::Trustline(ref tl) = entry.data {
                                    if let stellar_xdr::curr::TrustLineAsset::PoolShare(
                                        ref pool_id,
                                    ) = tl.asset
                                    {
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
                                    if let stellar_xdr::curr::TrustLineAsset::PoolShare(
                                        ref pool_id,
                                    ) = tl.asset
                                    {
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

        // Eagerly recompute Soroban network info from the committed bucket-list state.
        // Computed BEFORE acquiring the state write lock so readers never see a new
        // header paired with stale cached info.
        let soroban_info = self.compute_soroban_info();

        // Publish header, hash, and derived Soroban info atomically.
        {
            let mut state = self.state.write();
            state.header = new_header;
            state.header_hash = new_header_hash;
            state.soroban_network_info = soroban_info;
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
            components.push(ComponentMemory::new("bucket_list_cache", cache as u64, 0));
        }

        // Module cache
        if self.module_cache.read().is_some() {
            // Heuristic: compiled modules are ~4x their WASM size
            let soroban = self.soroban_state.read();
            let wasm_bytes = soroban.contract_code_state_size().max(0) as u64;
            let code_count = soroban.contract_code_count() as u64;
            let estimated_compiled = wasm_bytes * WASM_COMPILED_SIZE_MULTIPLIER;
            components.push(ComponentMemory::new(
                "module_cache",
                estimated_compiled,
                code_count,
            ));
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
                components.push(ComponentMemory::new("hot_archive_heap", heap as u64, 0));
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
        let key_hash = Hash256::hash_xdr(&data_key);
        let ttl_key = stellar_xdr::curr::LedgerKeyTtl {
            key_hash: stellar_xdr::curr::Hash(key_hash.0),
        };

        let mut state = self.soroban_state.write();
        state.create_contract_data(entry)?;
        state.create_ttl(&ttl_key, ttl_data)?;

        Ok(())
    }

    pub fn soroban_network_info(&self) -> Option<SorobanNetworkInfo> {
        self.state.read().soroban_network_info.clone()
    }

    /// Compute `SorobanNetworkInfo` from the current bucket-list state.
    ///
    /// Creates a snapshot and loads all ~15 ConfigSetting entries. This is
    /// called once per ledger close (on the blocking thread) and once during
    /// initialization. All subsequent reads go through the cached field in
    /// `LedgerState`.
    fn compute_soroban_info(&self) -> Option<SorobanNetworkInfo> {
        let snapshot = self.create_snapshot().ok()?;
        match load_soroban_network_info(&snapshot) {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(error = ?e, "Failed to compute Soroban network info");
                None
            }
        }
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

        let mut compiled = 0u32;
        let mut seen_hashes = std::collections::HashSet::<stellar_xdr::curr::Hash>::new();

        // Scan levels from 0 (newest) to 10 (oldest). Within each level,
        // curr shadows snap. Dead entries shadow live entries at higher levels.
        {
            let bucket_list = self.bucket_list.read();
            for level in bucket_list.levels() {
                for bucket in [level.curr.as_ref(), level.snap.as_ref()] {
                    let iter = match bucket.iter() {
                        Ok(it) => it,
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to iterate bucket during module cache rebuild");
                            continue;
                        }
                    };
                    for entry_result in iter {
                        let entry = match entry_result {
                            Ok(e) => e,
                            Err(e) => {
                                tracing::error!(error = %e, "Failed to read bucket entry during module cache rebuild");
                                continue;
                            }
                        };
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
        }

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
        let closing_ledger_seq = snapshot.ledger_seq() + 1;
        let protocol_version = snapshot.header().ledger_version;
        // Create a read-only CloseLedgerState so make_from_key can use the unified read path.
        let ltx = CloseLedgerState::begin(
            snapshot.clone(),
            snapshot.header().clone(),
            *snapshot.snapshot().header_hash(),
            closing_ledger_seq,
        );
        crate::config_upgrade::ConfigUpgradeSetFrame::make_from_key(
            &ltx,
            key,
            closing_ledger_seq,
            protocol_version,
        )
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
    ltx: CloseLedgerState,
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

impl LedgerCloseContext<'_> {
    /// Load an entry through the CloseLedgerState read path (current delta → snapshot).
    fn load_entry(&self, key: &LedgerKey) -> Result<Option<LedgerEntry>> {
        self.ltx.get_entry(key)
    }

    /// Load StateArchivalSettings through the CloseLedgerState read path.
    ///
    /// Parity: In stellar-core, the eviction scan runs after config upgrades are applied to the
    /// LedgerTxn, so it sees the upgraded StateArchival settings. CloseLedgerState's read path
    /// (current delta → snapshot) provides this automatically.
    fn load_state_archival_settings(&self) -> Option<StateArchivalSettings> {
        let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::StateArchival,
        });
        let entry = self.ltx.get_entry(&key).ok()??;
        if let LedgerEntryData::ConfigSetting(ConfigSettingEntry::StateArchival(settings)) =
            entry.data
        {
            Some(settings)
        } else {
            None
        }
    }

    /// Create the initial Soroban configuration entries for protocol v20.
    /// Apply version upgrade side effects (create config entries, cost types, etc.).
    ///
    /// Returns whether memory cost params were changed. Extracted as a helper
    /// so that the caller can wrap it in an error boundary matching stellar-core's
    /// per-upgrade try/catch (LedgerManagerImpl.cpp:1666-1690).
    fn apply_version_upgrade_side_effects(
        &mut self,
        prev_version: u32,
        protocol_version: u32,
    ) -> Result<bool> {
        let mut memory_cost_changed = false;

        // Parity: Upgrades.cpp:1189-1212
        if needs_upgrade_to_version(ProtocolVersion::V20, prev_version, protocol_version) {
            self.create_ledger_entries_for_v20()?;
            memory_cost_changed = true;
        }

        // Parity: Upgrades.cpp:1213-1217
        if needs_upgrade_to_version(ProtocolVersion::V21, prev_version, protocol_version) {
            self.create_cost_types_for_v21()?;
            memory_cost_changed = true;
        }

        // Parity: Upgrades.cpp:1219-1223
        if needs_upgrade_to_version(ProtocolVersion::V22, prev_version, protocol_version) {
            self.create_cost_types_for_v22()?;
            memory_cost_changed = true;
        }

        // Parity: Upgrades.cpp:1225-1229
        if needs_upgrade_to_version(ProtocolVersion::V23, prev_version, protocol_version) {
            self.create_and_update_ledger_entries_for_v23()?;
            memory_cost_changed = true;
        }

        // Parity: Upgrades.cpp:1229-1233
        if needs_upgrade_to_version(ProtocolVersion::V25, prev_version, protocol_version) {
            self.create_cost_types_for_v25()?;
            memory_cost_changed = true;
        }

        // Parity: NetworkConfig.cpp updateCostTypesForV26 + createLedgerEntriesForV26
        if needs_upgrade_to_version(ProtocolVersion::V26, prev_version, protocol_version) {
            self.update_cost_types_for_v26()?;
            self.create_ledger_entries_for_v26()?;
            memory_cost_changed = true;
        }

        // Parity: Upgrades.cpp:1189-1193
        // needUpgradeToVersion(V_10, prev, new) → prepareLiabilities
        // NOTE: Henyey supports protocol 24+ only, so prev_version < 10
        // should never be true in production. Included for completeness.
        if needs_upgrade_to_version(ProtocolVersion::V10, prev_version, protocol_version) {
            crate::prepare_liabilities::prepare_liabilities(
                &mut self.ltx,
                protocol_version,
                self.prev_header.base_reserve,
                self.close_data.ledger_seq,
            )?;
        }

        // Parity: Upgrades.cpp:1244-1251
        // prevVersion==V_23 && newVersion==V_24 && gIsProductionNetwork
        if prev_version == 23 && protocol_version == 24 && self.manager.network_id().is_mainnet() {
            self.ltx.record_fee_pool_delta(31_879_035);
            tracing::info!("Applied V24 mainnet fee pool correction: +31879035 stroops");
        }

        Ok(memory_cost_changed)
    }

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
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::ContractMaxSizeBytes(2_000)))?;

        // 2. CONFIG_SETTING_CONTRACT_DATA_KEY_SIZE_BYTES
        // Parity: NetworkConfig.cpp:60-74 initialMaxContractDataKeySizeEntry
        // MinimumSorobanNetworkConfig::MAX_CONTRACT_DATA_KEY_SIZE_BYTES = 200
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::ContractDataKeySizeBytes(
                200,
            )))?;

        // 3. CONFIG_SETTING_CONTRACT_DATA_ENTRY_SIZE_BYTES
        // Parity: NetworkConfig.cpp:76-90 initialMaxContractDataEntrySizeEntry
        // MinimumSorobanNetworkConfig::MAX_CONTRACT_DATA_ENTRY_SIZE_BYTES = 2000
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::ContractDataEntrySizeBytes(
                2_000,
            )))?;

        // 4. CONFIG_SETTING_CONTRACT_COMPUTE_V0
        // Parity: NetworkConfig.cpp:92-116 initialContractComputeSettingsEntry
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::ContractComputeV0(
                ConfigSettingContractComputeV0 {
                    // TX_MAX_INSTRUCTIONS = MinimumSorobanNetworkConfig::TX_MAX_INSTRUCTIONS = 2_500_000
                    ledger_max_instructions: 2_500_000, // LEDGER_MAX_INSTRUCTIONS = TX_MAX_INSTRUCTIONS
                    tx_max_instructions: 2_500_000,
                    fee_rate_per_instructions_increment: 100,
                    tx_memory_limit: 2_000_000, // MEMORY_LIMIT = MinimumSorobanNetworkConfig::MEMORY_LIMIT
                },
            )))?;

        // 5. CONFIG_SETTING_CONTRACT_LEDGER_COST_V0
        // Parity: NetworkConfig.cpp:118-175 initialContractLedgerAccessSettingsEntry
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::ContractLedgerCostV0(
                ConfigSettingContractLedgerCostV0 {
                    ledger_max_disk_read_entries: 3, // LEDGER_MAX_READ_LEDGER_ENTRIES = TX_MAX
                    ledger_max_disk_read_bytes: 3_200, // LEDGER_MAX_READ_BYTES = TX_MAX
                    ledger_max_write_ledger_entries: 2, // TX_MAX_WRITE_LEDGER_ENTRIES
                    ledger_max_write_bytes: 3_200,   // TX_MAX_WRITE_BYTES
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
                },
            )))?;

        // 6. CONFIG_SETTING_CONTRACT_HISTORICAL_DATA_V0
        // Parity: NetworkConfig.cpp:177-186 initialContractHistoricalDataSettingsEntry
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::ContractHistoricalDataV0(
                ConfigSettingContractHistoricalDataV0 {
                    fee_historical1_kb: 100,
                },
            )))?;

        // 7. CONFIG_SETTING_CONTRACT_EVENTS_V0
        // Parity: NetworkConfig.cpp:188-205 initialContractEventsSettingsEntry
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::ContractEventsV0(
                ConfigSettingContractEventsV0 {
                    tx_max_contract_events_size_bytes: 200, // MinimumSorobanNetworkConfig
                    fee_contract_events1_kb: 200,
                },
            )))?;

        // 8. CONFIG_SETTING_CONTRACT_BANDWIDTH_V0
        // Parity: NetworkConfig.cpp:207-227 initialContractBandwidthSettingsEntry
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::ContractBandwidthV0(
                ConfigSettingContractBandwidthV0 {
                    ledger_max_txs_size_bytes: 10_000, // TX_MAX_SIZE_BYTES = LEDGER_MAX
                    tx_max_size_bytes: 10_000,
                    fee_tx_size1_kb: 2_000,
                },
            )))?;

        // 9. CONFIG_SETTING_CONTRACT_EXECUTION_LANES
        // Parity: NetworkConfig.cpp:229-243 initialContractExecutionLanesSettingsEntry
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::ContractExecutionLanes(
                ConfigSettingContractExecutionLanesV0 {
                    ledger_max_tx_count: 1,
                },
            )))?;

        // 10. CONFIG_SETTING_CONTRACT_COST_PARAMS_CPU_INSTRUCTIONS
        // Parity: NetworkConfig.cpp:246-338 initialCpuCostParamsEntryForV20
        let cpu_params = Self::initial_cpu_cost_params_for_v20();
        self.ltx.record_create(make_entry(
            ConfigSettingEntry::ContractCostParamsCpuInstructions(ContractCostParams(
                cpu_params.try_into().map_err(|_| {
                    LedgerError::Internal("Failed to create V20 CPU cost params".to_string())
                })?,
            )),
        ))?;

        // 11. CONFIG_SETTING_CONTRACT_COST_PARAMS_MEMORY_BYTES
        // Parity: NetworkConfig.cpp:688-776 initialMemCostParamsEntryForV20
        let mem_params = Self::initial_mem_cost_params_for_v20();
        self.ltx.record_create(make_entry(
            ConfigSettingEntry::ContractCostParamsMemoryBytes(ContractCostParams(
                mem_params.try_into().map_err(|_| {
                    LedgerError::Internal("Failed to create V20 memory cost params".to_string())
                })?,
            )),
        ))?;

        // 12. CONFIG_SETTING_STATE_ARCHIVAL
        // Parity: NetworkConfig.cpp:632-685 initialStateArchivalSettings
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::StateArchival(
                StateArchivalSettings {
                    max_entry_ttl: 1_054_080,  // MAXIMUM_ENTRY_LIFETIME (61 days)
                    min_persistent_ttl: 4_096, // Live until level 6
                    min_temporary_ttl: 16,
                    persistent_rent_rate_denominator: 252_480,
                    temp_rent_rate_denominator: 2_524_800,
                    max_entries_to_archive: 100,
                    live_soroban_state_size_window_sample_size: 30,
                    live_soroban_state_size_window_sample_period: 64,
                    eviction_scan_size: 100_000, // 100 kb
                    starting_eviction_scan_level: 6,
                },
            )))?;

        // 13. CONFIG_SETTING_LIVE_SOROBAN_STATE_SIZE_WINDOW
        // Parity: NetworkConfig.cpp:1110-1126 initialliveSorobanStateSizeWindow
        // Populates 30-entry window with copies of current bucket list size.
        let bl_size = self
            .manager
            .bucket_list
            .read()
            .sum_bucket_entry_counters()
            .total_size();
        let window: Vec<u64> = vec![bl_size; 30]; // BUCKET_LIST_SIZE_WINDOW_SAMPLE_SIZE = 30
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::LiveSorobanStateSizeWindow(
                window.try_into().map_err(|_| {
                    LedgerError::Internal("Failed to create state size window".to_string())
                })?,
            )))?;

        // 14. CONFIG_SETTING_EVICTION_ITERATOR
        // Parity: NetworkConfig.cpp:1128-1139 initialEvictionIterator
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::EvictionIterator(
                EvictionIterator {
                    bucket_list_level: 6, // STARTING_EVICTION_SCAN_LEVEL
                    is_curr_bucket: true,
                    bucket_file_offset: 0,
                },
            )))?;

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
            e(4, 0),          // 0: WasmInsnExec
            e(434, 16),       // 1: MemAlloc
            e(42, 16),        // 2: MemCpy
            e(44, 16),        // 3: MemCmp
            e(310, 0),        // 4: DispatchHostFunction
            e(61, 0),         // 5: VisitObject
            e(230, 29),       // 6: ValSer
            e(59052, 4001),   // 7: ValDeser
            e(3738, 7012),    // 8: ComputeSha256Hash
            e(40253, 0),      // 9: ComputeEd25519PubKey
            e(377524, 4068),  // 10: VerifyEd25519Sig
            e(451626, 45405), // 11: VmInstantiation
            e(451626, 45405), // 12: VmCachedInstantiation
            e(1948, 0),       // 13: InvokeVmFunction
            e(3766, 5969),    // 14: ComputeKeccak256Hash
            e(710, 0),        // 15: DecodeEcdsaCurve256Sig
            e(2315295, 0),    // 16: RecoverEcdsaSecp256k1Key
            e(4404, 0),       // 17: Int256AddSub
            e(4947, 0),       // 18: Int256Mul
            e(4911, 0),       // 19: Int256Div
            e(4286, 0),       // 20: Int256Pow
            e(913, 0),        // 21: Int256Shift
            e(1058, 501),     // 22: ChaCha20DrawBytes
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
            e(0, 0),         // 0: WasmInsnExec
            e(16, 128),      // 1: MemAlloc
            e(0, 0),         // 2: MemCpy
            e(0, 0),         // 3: MemCmp
            e(0, 0),         // 4: DispatchHostFunction
            e(0, 0),         // 5: VisitObject
            e(242, 384),     // 6: ValSer
            e(0, 384),       // 7: ValDeser
            e(0, 0),         // 8: ComputeSha256Hash
            e(0, 0),         // 9: ComputeEd25519PubKey
            e(0, 0),         // 10: VerifyEd25519Sig
            e(130065, 5064), // 11: VmInstantiation
            e(130065, 5064), // 12: VmCachedInstantiation
            e(14, 0),        // 13: InvokeVmFunction
            e(0, 0),         // 14: ComputeKeccak256Hash
            e(0, 0),         // 15: DecodeEcdsaCurve256Sig
            e(181, 0),       // 16: RecoverEcdsaSecp256k1Key
            e(99, 0),        // 17: Int256AddSub
            e(99, 0),        // 18: Int256Mul
            e(99, 0),        // 19: Int256Div
            e(99, 0),        // 20: Int256Pow
            e(99, 0),        // 21: Int256Shift
            e(0, 0),         // 22: ChaCha20DrawBytes
        ]
    }

    /// Shared helper: load, resize, update, and persist CPU and memory cost param entries.
    ///
    /// Each update triple `(index, const_term, linear_term)` sets the corresponding
    /// `ContractCostParamEntry` at the given index. Indices beyond the current
    /// length are filled with zero entries during the resize.
    fn resize_and_update_cost_params(
        &mut self,
        new_size: usize,
        cpu_updates: &[(usize, i64, i64)],
        mem_updates: &[(usize, i64, i64)],
    ) -> Result<()> {
        use stellar_xdr::curr::{ContractCostParamEntry, ContractCostParams};

        let make_entry = |const_term: i64, linear_term: i64| ContractCostParamEntry {
            ext: ExtensionPoint::V0,
            const_term,
            linear_term,
        };

        // Helper closure: load, resize, update, and persist one cost param config setting.
        let mut update_params = |setting_id: ConfigSettingId,
                                 updates: &[(usize, i64, i64)],
                                 wrap: fn(ContractCostParams) -> ConfigSettingEntry,
                                 label: &str|
         -> Result<()> {
            let key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
                config_setting_id: setting_id,
            });
            let entry = self
                .load_entry(&key)?
                .ok_or_else(|| LedgerError::Internal(format!("{label} entry not found")))?;

            let mut params = if let LedgerEntryData::ConfigSetting(ref cs) = entry.data {
                match cs {
                    ConfigSettingEntry::ContractCostParamsCpuInstructions(p)
                    | ConfigSettingEntry::ContractCostParamsMemoryBytes(p) => p.0.to_vec(),
                    _ => {
                        return Err(LedgerError::Internal(format!(
                            "Unexpected entry type for {label}"
                        )));
                    }
                }
            } else {
                return Err(LedgerError::Internal(format!(
                    "Unexpected entry type for {label}"
                )));
            };

            params.resize(new_size, make_entry(0, 0));
            for &(idx, c, l) in updates {
                params[idx] = make_entry(c, l);
            }

            let new_entry = LedgerEntry {
                last_modified_ledger_seq: self.close_data.ledger_seq,
                data: LedgerEntryData::ConfigSetting(wrap(ContractCostParams(
                    params
                        .try_into()
                        .map_err(|_| LedgerError::Internal(format!("Failed to convert {label}")))?,
                ))),
                ext: LedgerEntryExt::V0,
            };
            self.ltx.record_update(entry, new_entry)?;
            Ok(())
        };

        update_params(
            ConfigSettingId::ContractCostParamsCpuInstructions,
            cpu_updates,
            ConfigSettingEntry::ContractCostParamsCpuInstructions,
            "ContractCostParamsCpuInstructions",
        )?;

        update_params(
            ConfigSettingId::ContractCostParamsMemoryBytes,
            mem_updates,
            ConfigSettingEntry::ContractCostParamsMemoryBytes,
            "ContractCostParamsMemoryBytes",
        )?;

        Ok(())
    }

    /// Apply version upgrade side effects for protocol 21.
    ///
    /// Parity: NetworkConfig.cpp:1432-1439 `createCostTypesForV21`
    /// Resizes CPU and memory cost params to include ParseWasm/InstantiateWasm types
    /// and ECDSA-secp256r1 verification.
    fn create_cost_types_for_v21(&mut self) -> Result<()> {
        const NEW_SIZE: usize = 45; // V21 last cost type: VerifyEcdsaSecp256r1Sig = 44

        // CPU params: Parity: NetworkConfig.cpp:340-441 updateCpuCostParamsEntryForV21
        let cpu_updates: &[(usize, i64, i64)] = &[
            (12, 41142, 634),   // VmCachedInstantiation (updated)
            (23, 73077, 25410), // ParseWasmInstructions
            (24, 0, 540752),    // ParseWasmFunctions
            (25, 0, 176363),    // ParseWasmGlobals
            (26, 0, 29989),     // ParseWasmTableEntries
            (27, 0, 1061449),   // ParseWasmTypes
            (28, 0, 237336),    // ParseWasmDataSegments
            (29, 0, 328476),    // ParseWasmElemSegments
            (30, 0, 701845),    // ParseWasmImports
            (31, 0, 429383),    // ParseWasmExports
            (32, 0, 28),        // ParseWasmDataSegmentBytes
            (33, 43030, 0),     // InstantiateWasmInstructions
            (34, 0, 7556),      // InstantiateWasmFunctions
            (35, 0, 10711),     // InstantiateWasmGlobals
            (36, 0, 3300),      // InstantiateWasmTableEntries
            (37, 0, 0),         // InstantiateWasmTypes
            (38, 0, 23038),     // InstantiateWasmDataSegments
            (39, 0, 42488),     // InstantiateWasmElemSegments
            (40, 0, 828974),    // InstantiateWasmImports
            (41, 0, 297100),    // InstantiateWasmExports
            (42, 0, 14),        // InstantiateWasmDataSegmentBytes
            (43, 1882, 0),      // Sec1DecodePointUncompressed
            (44, 3000906, 0),   // VerifyEcdsaSecp256r1Sig
        ];

        // Memory params: Parity: NetworkConfig.cpp:778-880 updateMemCostParamsEntryForV21
        let mem_updates: &[(usize, i64, i64)] = &[
            (12, 69472, 1217), // VmCachedInstantiation (updated)
            (23, 17564, 6457), // ParseWasmInstructions
            (24, 0, 47464),    // ParseWasmFunctions
            (25, 0, 13420),    // ParseWasmGlobals
            (26, 0, 6285),     // ParseWasmTableEntries
            (27, 0, 64670),    // ParseWasmTypes
            (28, 0, 29074),    // ParseWasmDataSegments
            (29, 0, 48095),    // ParseWasmElemSegments
            (30, 0, 103229),   // ParseWasmImports
            (31, 0, 36394),    // ParseWasmExports
            (32, 0, 257),      // ParseWasmDataSegmentBytes
            (33, 70704, 0),    // InstantiateWasmInstructions
            (34, 0, 14613),    // InstantiateWasmFunctions
            (35, 0, 6833),     // InstantiateWasmGlobals
            (36, 0, 1025),     // InstantiateWasmTableEntries
            (37, 0, 0),        // InstantiateWasmTypes
            (38, 0, 129632),   // InstantiateWasmDataSegments
            (39, 0, 13665),    // InstantiateWasmElemSegments
            (40, 0, 97637),    // InstantiateWasmImports
            (41, 0, 9176),     // InstantiateWasmExports
            (42, 0, 126),      // InstantiateWasmDataSegmentBytes
            (43, 0, 0),        // Sec1DecodePointUncompressed
            (44, 0, 0),        // VerifyEcdsaSecp256r1Sig
        ];

        self.resize_and_update_cost_params(NEW_SIZE, cpu_updates, mem_updates)?;

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
        const NEW_SIZE: usize = 70; // V22 last cost type: Bls12381FrInv = 69

        // CPU params: Parity: NetworkConfig.cpp:443-553 updateCpuCostParamsEntryForV22
        let cpu_updates: &[(usize, i64, i64)] = &[
            (45, 661, 0),              // Bls12381EncodeFp
            (46, 985, 0),              // Bls12381DecodeFp
            (47, 1934, 0),             // Bls12381G1CheckPointOnCurve
            (48, 730510, 0),           // Bls12381G1CheckPointInSubgroup
            (49, 5921, 0),             // Bls12381G2CheckPointOnCurve
            (50, 1057822, 0),          // Bls12381G2CheckPointInSubgroup
            (51, 92642, 0),            // Bls12381G1ProjectiveToAffine
            (52, 100742, 0),           // Bls12381G2ProjectiveToAffine
            (53, 7689, 0),             // Bls12381G1Add
            (54, 2458985, 0),          // Bls12381G1Mul
            (55, 2426722, 96397671),   // Bls12381G1Msm
            (56, 1541554, 0),          // Bls12381MapFpToG1
            (57, 3211191, 6713),       // Bls12381HashToG1
            (58, 25207, 0),            // Bls12381G2Add
            (59, 7873219, 0),          // Bls12381G2Mul
            (60, 8035968, 309667335),  // Bls12381G2Msm
            (61, 2420202, 0),          // Bls12381MapFp2ToG2
            (62, 7050564, 6797),       // Bls12381HashToG2
            (63, 10558948, 632860943), // Bls12381Pairing
            (64, 1994, 0),             // Bls12381FrFromU256
            (65, 1155, 0),             // Bls12381FrToU256
            (66, 74, 0),               // Bls12381FrAddSub
            (67, 332, 0),              // Bls12381FrMul
            (68, 691, 74558),          // Bls12381FrPow
            (69, 35421, 0),            // Bls12381FrInv
        ];

        // Memory params: Parity: NetworkConfig.cpp:882-990 updateMemCostParamsEntryForV22
        let mem_updates: &[(usize, i64, i64)] = &[
            (45, 0, 0),           // Bls12381EncodeFp
            (46, 0, 0),           // Bls12381DecodeFp
            (47, 0, 0),           // Bls12381G1CheckPointOnCurve
            (48, 0, 0),           // Bls12381G1CheckPointInSubgroup
            (49, 0, 0),           // Bls12381G2CheckPointOnCurve
            (50, 0, 0),           // Bls12381G2CheckPointInSubgroup
            (51, 0, 0),           // Bls12381G1ProjectiveToAffine
            (52, 0, 0),           // Bls12381G2ProjectiveToAffine
            (53, 0, 0),           // Bls12381G1Add
            (54, 0, 0),           // Bls12381G1Mul
            (55, 109494, 354667), // Bls12381G1Msm
            (56, 5552, 0),        // Bls12381MapFpToG1
            (57, 9424, 0),        // Bls12381HashToG1
            (58, 0, 0),           // Bls12381G2Add
            (59, 0, 0),           // Bls12381G2Mul
            (60, 219654, 354667), // Bls12381G2Msm
            (61, 3344, 0),        // Bls12381MapFp2ToG2
            (62, 6816, 0),        // Bls12381HashToG2
            (63, 2204, 9340474),  // Bls12381Pairing
            (64, 0, 0),           // Bls12381FrFromU256
            (65, 248, 0),         // Bls12381FrToU256
            (66, 0, 0),           // Bls12381FrAddSub
            (67, 0, 0),           // Bls12381FrMul
            (68, 0, 128),         // Bls12381FrPow
            (69, 0, 0),           // Bls12381FrInv
        ];

        self.resize_and_update_cost_params(NEW_SIZE, cpu_updates, mem_updates)?;

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
        self.ltx.record_create(LedgerEntry {
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
        self.ltx.record_create(LedgerEntry {
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

        self.ltx.record_create(LedgerEntry {
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
            self.ltx.record_update(cost_entry, new_entry)?;
        } else {
            return Err(LedgerError::Internal(
                "Unexpected entry type for ContractLedgerCostV0".to_string(),
            ));
        }

        // Update CONFIG_SETTING_STATE_ARCHIVAL
        let archival_key = LedgerKey::ConfigSetting(LedgerKeyConfigSetting {
            config_setting_id: ConfigSettingId::StateArchival,
        });
        let archival_entry = self
            .load_entry(&archival_key)?
            .ok_or_else(|| LedgerError::Internal("StateArchival entry not found".to_string()))?;

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
            self.ltx.record_update(archival_entry, new_entry)?;
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

        // CPU params: from NetworkConfig.cpp:556-629
        let cpu_updates: &[(usize, i64, i64)] = &[
            (BN254_ENCODE_FP, 344, 0),
            (BN254_DECODE_FP, 476, 0),
            (BN254_G1_CHECK_POINT_ON_CURVE, 904, 0),
            (BN254_G2_CHECK_POINT_ON_CURVE, 2811, 0),
            (BN254_G2_CHECK_POINT_IN_SUBGROUP, 2937755, 0),
            (BN254_G1_PROJECTIVE_TO_AFFINE, 61, 0),
            (BN254_G1_ADD, 3623, 0),
            (BN254_G1_MUL, 1150435, 0),
            (BN254_PAIRING, 5263916, 392472814),
            (BN254_FR_FROM_U256, 2052, 0),
            (BN254_FR_TO_U256, 1133, 0),
            (BN254_FR_ADD_SUB, 74, 0),
            (BN254_FR_MUL, 332, 0),
            (BN254_FR_POW, 755, 68930),
            (BN254_FR_INV, 33151, 0),
        ];

        // Memory params: from NetworkConfig.cpp:993-1067
        let mem_updates: &[(usize, i64, i64)] = &[
            (BN254_ENCODE_FP, 0, 0),
            (BN254_DECODE_FP, 0, 0),
            (BN254_G1_CHECK_POINT_ON_CURVE, 0, 0),
            (BN254_G2_CHECK_POINT_ON_CURVE, 0, 0),
            (BN254_G2_CHECK_POINT_IN_SUBGROUP, 0, 0),
            (BN254_G1_PROJECTIVE_TO_AFFINE, 0, 0),
            (BN254_G1_ADD, 0, 0),
            (BN254_G1_MUL, 0, 0),
            (BN254_PAIRING, 1821, 6232546),
            (BN254_FR_FROM_U256, 0, 0),
            (BN254_FR_TO_U256, 312, 0),
            (BN254_FR_ADD_SUB, 0, 0),
            (BN254_FR_MUL, 0, 0),
            (BN254_FR_POW, 0, 0),
            (BN254_FR_INV, 0, 0),
        ];

        self.resize_and_update_cost_params(NEW_SIZE, cpu_updates, mem_updates)?;

        tracing::info!(
            ledger_seq = self.close_data.ledger_seq,
            new_size = NEW_SIZE,
            "Applied createCostTypesForV25: resized cost params with BN254 entries"
        );

        Ok(())
    }

    /// Update cost type parameters for Protocol 26.
    ///
    /// Parity: NetworkConfig.cpp updateCpuCostParamsEntryForV26 + updateMemCostParamsEntryForV26
    /// Resizes cost params from 85 → 86 entries (adds Bn254G1Msm at index 85)
    /// and updates BLS12-381 and BN254 cost type values.
    fn update_cost_types_for_v26(&mut self) -> Result<()> {
        // Cost type indices (from ContractCostType enum)
        const BLS12381_G1_MSM: usize = 55;
        const BLS12381_MAP_FP_TO_G1: usize = 56;
        const BLS12381_HASH_TO_G1: usize = 57;
        const BLS12381_G2_MSM: usize = 60;
        const BLS12381_MAP_FP2_TO_G2: usize = 61;
        const BLS12381_HASH_TO_G2: usize = 62;
        const BN254_G2_CHECK_POINT_IN_SUBGROUP: usize = 74;
        const BN254_G1_MSM: usize = 85;
        const NEW_SIZE: usize = BN254_G1_MSM + 1; // 86

        // CPU params: from NetworkConfig.cpp:635-694
        let cpu_updates: &[(usize, i64, i64)] = &[
            (BLS12381_G1_MSM, 2347584, 94135478),
            (BLS12381_MAP_FP_TO_G1, 1020885, 0),
            (BLS12381_HASH_TO_G1, 2638451, 6803),
            (BLS12381_G2_MSM, 7663880, 298580871),
            (BLS12381_MAP_FP2_TO_G2, 1856539, 0),
            (BLS12381_HASH_TO_G2, 6315452, 7232),
            (BN254_G2_CHECK_POINT_IN_SUBGROUP, 1706052, 0),
            (BN254_G1_MSM, 1185193, 41568084),
        ];

        // Memory params: from NetworkConfig.cpp:1135-1190
        let mem_updates: &[(usize, i64, i64)] = &[
            (BLS12381_G1_MSM, 109494, 266603),
            (BLS12381_MAP_FP_TO_G1, 2776, 0),
            (BLS12381_HASH_TO_G1, 5896, 0),
            (BLS12381_G2_MSM, 219654, 266603),
            (BLS12381_MAP_FP2_TO_G2, 1672, 0),
            (BLS12381_HASH_TO_G2, 3960, 0),
            (BN254_G1_MSM, 73061, 229779),
        ];

        self.resize_and_update_cost_params(NEW_SIZE, cpu_updates, mem_updates)?;

        tracing::info!(
            ledger_seq = self.close_data.ledger_seq,
            new_size = NEW_SIZE,
            "Applied updateCostTypesForV26: updated BLS12-381/BN254 cost params and added Bn254G1Msm"
        );

        Ok(())
    }

    /// Create initial CONFIG_SETTING entries for Protocol 26 (CAP-77 frozen ledger keys).
    ///
    /// Parity: NetworkConfig.cpp createLedgerEntriesForV26
    /// Creates 2 CONFIG_SETTING entries with empty frozen key and bypass tx sets.
    fn create_ledger_entries_for_v26(&mut self) -> Result<()> {
        use stellar_xdr::curr::{FreezeBypassTxs, FrozenLedgerKeys, VecM};

        let ledger_seq = self.close_data.ledger_seq;
        let make_entry = |config: ConfigSettingEntry| -> LedgerEntry {
            LedgerEntry {
                last_modified_ledger_seq: ledger_seq,
                data: LedgerEntryData::ConfigSetting(config),
                ext: LedgerEntryExt::V0,
            }
        };

        // CONFIG_SETTING_FROZEN_LEDGER_KEYS (empty)
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::FrozenLedgerKeys(
                FrozenLedgerKeys {
                    keys: VecM::default(),
                },
            )))?;

        // CONFIG_SETTING_FREEZE_BYPASS_TXS (empty)
        self.ltx
            .record_create(make_entry(ConfigSettingEntry::FreezeBypassTxs(
                FreezeBypassTxs {
                    tx_hashes: VecM::default(),
                },
            )))?;

        tracing::info!(
            ledger_seq,
            "Applied createLedgerEntriesForV26: created 2 CONFIG_SETTING entries for frozen keys"
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
    /// advanced via `advance_to_ledger` which clears non-offer
    /// cached entries while keeping the offer index intact.
    fn apply_transactions(&mut self) -> Result<Vec<TransactionExecutionResult>> {
        use henyey_common::protocol::PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION;

        let prepare_start = std::time::Instant::now();
        let tx_set_hash = self.close_data.tx_set_hash();
        let prepared = if self.close_data.presorted == SortState::Presorted {
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

        // Load SorobanConfig from ledger ConfigSettingEntry for accurate Soroban execution.
        // Only loaded for protocol >= 20 (Soroban protocol), matching stellar-core's guard
        // in LedgerManagerImpl which only calls loadFromLedger for Soroban protocol versions.
        let config_load_start = std::time::Instant::now();
        let soroban_config = if protocol_version_starts_from(
            self.prev_header.ledger_version,
            ProtocolVersion::V20,
        ) {
            crate::execution::load_soroban_config(&self.ltx, self.prev_header.ledger_version)?
        } else {
            henyey_tx::soroban::SorobanConfig::default()
        };
        // Cache fee_write_1kb for LedgerCloseMetaExtV1 (set during commit phase).
        // This is stellar-core's feeRent1KB() / sorobanFeeWrite1KB.
        self.soroban_fee_write_1kb = soroban_config.rent_fee_config.fee_per_write_1kb;
        // Load frozen key configuration (CAP-77, Protocol 26+).
        let frozen_key_config =
            crate::execution::load_frozen_key_config(&self.ltx, self.prev_header.ledger_version)?;
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
        let id_pool = self.ltx.snapshot().header().id_pool;

        let executor_ref = executor.get_or_insert_with(|| {
            let mut ctx = LedgerContext::new(
                self.close_data.ledger_seq,
                self.close_data.close_time,
                self.prev_header.base_fee,
                self.prev_header.base_reserve,
                self.prev_header.ledger_version,
                self.manager.network_id,
            );
            ctx.frozen_key_config = frozen_key_config.clone();
            TransactionExecutor::new(&ctx, id_pool, soroban_config.clone(), classic_events)
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
            executor_ref.advance_to_ledger(LedgerAdvanceParams {
                ledger_seq: self.close_data.ledger_seq,
                close_time: self.close_data.close_time,
                base_reserve: self.prev_header.base_reserve,
                protocol_version: self.prev_header.ledger_version,
                soroban_config: soroban_config.clone(),
                frozen_key_config: frozen_key_config.clone(),
                ledger_flags: match &self.prev_header.ext {
                    stellar_xdr::curr::LedgerHeaderExt::V0 => 0,
                    stellar_xdr::curr::LedgerHeaderExt::V1(ext) => ext.flags,
                },
            });
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

        // Clone snapshot for passing alongside mutable delta to execution functions.
        // SnapshotHandle is Arc-based so this is a cheap reference count increment.
        let snapshot_clone = self.ltx.snapshot().clone();

        let mut fee_pre_deduct_us: u64 = 0;
        let mut tx_set_result = if has_parallel
            && protocol_version_starts_from(
                self.prev_header.ledger_version,
                PARALLEL_SOROBAN_PHASE_PROTOCOL_VERSION,
            ) {
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
                    self.ltx.current_delta_mut(),
                    &snapshot_clone,
                )?;
            self.ltx.record_fee_pool_delta(total_fee_pool);

            // Pre-load fee-deducted account entries from the delta into the
            // classic executor so classic TXs see ALL fee deductions (including
            // Soroban fees on shared accounts).
            for entry in self.ltx.current_delta().current_entries() {
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
                    id_pool: snapshot_clone.header().id_pool,
                    hot_archive_restored_keys: Vec::new(),
                }
            } else {
                run_transactions_on_executor(crate::execution::RunTransactionsParams {
                    executor: executor_ref,
                    snapshot: &snapshot_clone,
                    transactions: classic_txs,
                    base_fee: self.prev_header.base_fee,
                    soroban_base_prng_seed: soroban_base_prng_seed.0,
                    deduct_fee: false,
                    delta: self.ltx.current_delta_mut(),
                    external_pre_charged: Some(&classic_pre_charged),
                })?
            };
            self.timing_classic_exec_us = classic_start.elapsed().as_micros() as u64;

            // Execute Soroban parallel phase (fees already deducted on delta).
            let soroban_start = std::time::Instant::now();
            let mut ledger_context = LedgerContext::new(
                self.close_data.ledger_seq,
                self.close_data.close_time,
                self.prev_header.base_fee,
                self.prev_header.base_reserve,
                self.prev_header.ledger_version,
                self.manager.network_id,
            );
            ledger_context.frozen_key_config = frozen_key_config.clone();
            let soroban_result = execute_soroban_parallel_phase(
                &snapshot_clone,
                phase,
                classic_txs.len(),
                &ledger_context,
                self.ltx.current_delta_mut(),
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
                    enable_soroban_diagnostic_events: self
                        .manager
                        .config
                        .enable_soroban_diagnostic_events,
                },
                Some(soroban_pre_charged),
            )?;
            self.timing_soroban_exec_us = soroban_start.elapsed().as_micros() as u64;

            // Combine results: classic first, then Soroban.
            classic_result.results.extend(soroban_result.results);
            classic_result.tx_results.extend(soroban_result.tx_results);
            classic_result
                .tx_result_metas
                .extend(soroban_result.tx_result_metas);
            classic_result.id_pool = classic_result.id_pool.max(soroban_result.id_pool);
            classic_result
                .hot_archive_restored_keys
                .extend(soroban_result.hot_archive_restored_keys);

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
            run_transactions_on_executor(crate::execution::RunTransactionsParams {
                executor: executor_ref,
                snapshot: &snapshot_clone,
                transactions: &prepared.all_txs,
                base_fee: self.prev_header.base_fee,
                soroban_base_prng_seed: soroban_base_prng_seed.0,
                deduct_fee: true,
                delta: self.ltx.current_delta_mut(),
                external_pre_charged: None,
            })?
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
        let op_count: usize = tx_set_result
            .results
            .iter()
            .map(|r| r.operation_results.len())
            .sum();
        let fees_collected: i64 = tx_set_result.results.iter().map(|r| r.fee_charged).sum();

        self.stats
            .record_transactions(tx_count, success_count, op_count);
        self.stats.record_fees(fees_collected);

        // Collect per-transaction perf data and aggregate sub-phase timings
        let mut agg_op_type_timings: HashMap<henyey_tx::OperationType, (u64, u32)> = HashMap::new();
        let (
            mut agg_validation_us,
            mut agg_fee_seq_us,
            mut agg_footprint_us,
            mut agg_ops_us,
            mut agg_meta_build_us,
        ) = (0u64, 0u64, 0u64, 0u64, 0u64);
        let (
            mut agg_val_account_load_us,
            mut agg_val_tx_hash_us,
            mut agg_val_ed25519_us,
            mut agg_val_other_us,
        ) = (0u64, 0u64, 0u64, 0u64);
        let (
            mut agg_fee_deduct_us,
            mut agg_op_sig_check_us,
            mut agg_signer_removal_us,
            mut agg_seq_bump_us,
        ) = (0u64, 0u64, 0u64, 0u64);
        for (i, result) in tx_set_result.results.iter().enumerate() {
            let hash_hex = if i < self.tx_results.len() {
                Hash256::from_bytes(self.tx_results[i].transaction_hash.0).to_hex()[..16]
                    .to_string()
            } else {
                String::new()
            };
            let is_soroban = prepared.tx_meta.get(i).map_or(false, |m| m.is_soroban);
            self.tx_perf.push(crate::close::TxPerf {
                index: i,
                hash_hex,
                success: result.success,
                op_count: result.operation_results.len(),
                exec_us: result.timings.exec_time_us,
                is_soroban,
            });
            // Aggregate per-TX sub-phase timings
            agg_validation_us += result.timings.validation_us;
            agg_fee_seq_us += result.timings.fee_seq_us;
            agg_footprint_us += result.timings.footprint_us;
            agg_ops_us += result.timings.ops_us;
            agg_meta_build_us += result.timings.meta_build_us;
            agg_val_account_load_us += result.timings.val_account_load_us;
            agg_val_tx_hash_us += result.timings.val_tx_hash_us;
            agg_val_ed25519_us += result.timings.val_ed25519_us;
            agg_val_other_us += result.timings.val_other_us;
            agg_fee_deduct_us += result.timings.fee_deduct_us;
            agg_op_sig_check_us += result.timings.op_sig_check_us;
            agg_signer_removal_us += result.timings.signer_removal_us;
            agg_seq_bump_us += result.timings.seq_bump_us;
            // Aggregate per-op-type timings across all TXs
            for (op_type, (us, count)) in &result.timings.op_type_timings {
                let entry = agg_op_type_timings.entry(*op_type).or_insert((0, 0));
                entry.0 += us;
                entry.1 += count;
            }
        }
        let post_exec_us = post_exec_start.elapsed().as_micros() as u64;

        // Build per-op-type timing summary sorted by time desc
        let mut op_timing_vec: Vec<_> = agg_op_type_timings.iter().collect();
        op_timing_vec.sort_by_key(|a| std::cmp::Reverse(a.1 .0));
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
        use stellar_xdr::curr::{LedgerEntryChanges, LedgerUpgrade, Limits, WriteXdr};

        // Parity: Upgrades.cpp:1229-1242 applyVersionUpgrade
        // Version upgrades may create/modify config setting entries in the
        // ledger (e.g. new cost types for V25, state size window for V23+).
        // These must be applied to the delta before bucket list extraction.
        //
        // Parity: stellar-core wraps each upgrade in a per-upgrade try/catch
        // (LedgerManagerImpl.cpp:1666-1690) that logs errors and continues.
        // We mirror that: errors are logged and skipped rather than aborting
        // the ledger close.
        let mut version_upgrade_memory_cost_changed = false;
        let mut version_upgrade_succeeded = prev_version == protocol_version;

        // Capture changes from version upgrade side effects (cost types for V25).
        let version_changes = if prev_version != protocol_version {
            let cp = self.ltx.change_checkpoint();
            match self.apply_version_upgrade_side_effects(prev_version, protocol_version) {
                Ok(memory_cost_changed) => {
                    version_upgrade_memory_cost_changed = memory_cost_changed;
                    version_upgrade_succeeded = true;
                }
                Err(e) => {
                    tracing::error!(
                        prev_version,
                        protocol_version,
                        error = %e,
                        "Exception during version upgrade side effects — skipping"
                    );
                }
            }
            // Extract changes made during version upgrade side effects
            self.ltx.entry_changes_since(cp)
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
        // would run prepareLiabilities twice (once per upgrade). With CloseLedgerState,
        // both passes would see each other's changes through the read path.
        let mut reserve_upgrade_succeeded = true;
        let reserve_changes = if let Some(new_reserve) = self.upgrade_ctx.base_reserve_upgrade() {
            let did_reserve_increase = new_reserve > self.prev_header.base_reserve;
            if protocol_version_starts_from(protocol_version, ProtocolVersion::V10)
                && did_reserve_increase
            {
                let cp = self.ltx.change_checkpoint();
                match crate::prepare_liabilities::prepare_liabilities(
                    &mut self.ltx,
                    protocol_version,
                    new_reserve,
                    self.close_data.ledger_seq,
                ) {
                    Ok(()) => self.ltx.entry_changes_since(cp),
                    Err(e) => {
                        tracing::error!(
                            new_reserve,
                            error = %e,
                            "Exception during reserve upgrade (prepareLiabilities) — skipping"
                        );
                        reserve_upgrade_succeeded = false;
                        LedgerEntryChanges(VecM::default())
                    }
                }
            } else {
                LedgerEntryChanges(VecM::default())
            }
        } else {
            LedgerEntryChanges(VecM::default())
        };

        // Apply config upgrades through CloseLedgerState BEFORE extracting entries for the bucket list.
        // In stellar-core, config upgrades are applied to the LedgerTxn before
        // getAllEntries() and addBatch(), so the upgraded ConfigSetting entries are included
        // in the bucket list update. We must do the same here.
        let mut config_state_archival_changed = false;
        let mut config_memory_cost_params_changed = false;
        let mut per_config_changes: HashMap<Vec<u8>, LedgerEntryChanges> = HashMap::new();
        let mut config_upgrade_succeeded = true;
        let delta_count_before_upgrades = self.ltx.num_changes();
        if self.upgrade_ctx.has_config_upgrades() {
            match self.upgrade_ctx.apply_config_upgrades(
                &mut self.ltx,
                self.close_data.ledger_seq,
                protocol_version,
            ) {
                Ok(result) => {
                    config_state_archival_changed = result.state_archival_changed;
                    config_memory_cost_params_changed = result.memory_cost_params_changed;
                    per_config_changes = result.per_upgrade_changes;
                    tracing::info!(
                        ledger_seq = self.close_data.ledger_seq,
                        delta_before = delta_count_before_upgrades,
                        delta_after = self.ltx.num_changes(),
                        archival_changed = config_state_archival_changed,
                        memory_cost_changed = config_memory_cost_params_changed,
                        "Delta entry count after config upgrades"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        ledger_seq = self.close_data.ledger_seq,
                        error = %e,
                        "Exception during config upgrade — skipping"
                    );
                    config_upgrade_succeeded = false;
                }
            }
        }

        // Apply MaxSorobanTxSetSize upgrade through CloseLedgerState (modifies CONFIG_SETTING entry).
        // Parity: Upgrades.cpp upgradeMaxSorobanTxSetSize()
        let mut max_soroban_upgrade_succeeded = true;
        let max_soroban_changes = if self.upgrade_ctx.max_soroban_tx_set_size_upgrade().is_some() {
            match self
                .upgrade_ctx
                .apply_max_soroban_tx_set_size(&mut self.ltx, self.close_data.ledger_seq)
            {
                Ok(changes) => changes,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "Exception during MaxSorobanTxSetSize upgrade — skipping"
                    );
                    max_soroban_upgrade_succeeded = false;
                    LedgerEntryChanges(VecM::default())
                }
            }
        } else {
            LedgerEntryChanges(VecM::default())
        };

        // Build UpgradeEntryMeta only for upgrades that succeeded.
        // Parity: LedgerManagerImpl.cpp:1671-1680 — stellar-core only appends
        // meta after a successful child-txn commit; failed upgrades produce no meta.
        let mut upgrades_meta = Vec::new();
        for upgrade in std::mem::take(&mut self.close_data.upgrades) {
            let (succeeded, changes) = match &upgrade {
                LedgerUpgrade::Version(_) => (version_upgrade_succeeded, version_changes.clone()),
                LedgerUpgrade::Config(key) => {
                    if config_upgrade_succeeded {
                        let key_bytes = key
                            .to_xdr(Limits::none())
                            .expect("XDR encoding of ConfigSettingId must not fail");
                        let changes = per_config_changes
                            .remove(&key_bytes)
                            .unwrap_or_else(|| LedgerEntryChanges(VecM::default()));
                        (true, changes)
                    } else {
                        (false, LedgerEntryChanges(VecM::default()))
                    }
                }
                LedgerUpgrade::MaxSorobanTxSetSize(_) => {
                    (max_soroban_upgrade_succeeded, max_soroban_changes.clone())
                }
                LedgerUpgrade::BaseReserve(_) => {
                    (reserve_upgrade_succeeded, reserve_changes.clone())
                }
                // BaseFee, MaxTxSetSize, Flags — header-only, always succeed
                _ => (true, LedgerEntryChanges(VecM::default())),
            };
            if succeeded {
                upgrades_meta.push(UpgradeEntryMeta { upgrade, changes });
            } else {
                tracing::warn!(
                    upgrade_type = ?std::mem::discriminant(&upgrade),
                    "Omitting UpgradeEntryMeta for failed upgrade"
                );
            }
        }

        // Parity: Upgrades.cpp:1238-1242 and 1449-1453
        // handleUpgradeAffectingSorobanInMemoryStateSize is called:
        // 1. After version upgrade to V23+ (recompute with potentially new cost params)
        // 2. After config upgrade that changes ContractCostParamsMemoryBytes
        // It recomputes contract code sizes in-memory and overwrites all window entries.
        let version_upgrade_triggers_state_size = prev_version != protocol_version
            && protocol_version_starts_from(protocol_version, ProtocolVersion::V23);
        if (config_memory_cost_params_changed
            || version_upgrade_memory_cost_changed
            || version_upgrade_triggers_state_size)
            && protocol_version >= henyey_common::MIN_SOROBAN_PROTOCOL_VERSION
        {
            // Load rent config through CloseLedgerState (sees post-upgrade values).
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
            if protocol_version_starts_from(protocol_version, ProtocolVersion::V23) {
                let new_size = self.manager.soroban_state.read().total_size();
                let window_key = stellar_xdr::curr::LedgerKey::ConfigSetting(
                    stellar_xdr::curr::LedgerKeyConfigSetting {
                        config_setting_id:
                            stellar_xdr::curr::ConfigSettingId::LiveSorobanStateSizeWindow,
                    },
                );

                // Read the window through CloseLedgerState (may have been resized by config upgrade).
                // Parity: stellar-core reads from LedgerTxn which includes prior modifications.
                let (window_vec_base, previous_entry) = match self.ltx.get_entry(&window_key)? {
                    Some(entry) => {
                        if let stellar_xdr::curr::LedgerEntryData::ConfigSetting(
                            stellar_xdr::curr::ConfigSettingEntry::LiveSorobanStateSizeWindow(
                                ref w,
                            ),
                        ) = entry.data
                        {
                            (Some(w.iter().copied().collect::<Vec<u64>>()), Some(entry))
                        } else {
                            (None, None)
                        }
                    }
                    None => (None, None),
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
                    self.ltx.record_update(prev, new_window_entry)?;
                    tracing::info!(
                        ledger_seq = self.close_data.ledger_seq,
                        new_size = new_size,
                        delta_count = self.ltx.num_changes(),
                        "Updated all state size window entries due to memory cost params upgrade"
                    );
                }
            }
        }

        Ok((
            config_state_archival_changed,
            config_memory_cost_params_changed,
            upgrades_meta,
        ))
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
        let total_coins = self.prev_header.total_coins + self.ltx.total_coins_delta();
        let fee_pool = self.prev_header.fee_pool + self.ltx.fee_pool_delta();
        tracing::debug!(
            ledger_seq = self.close_data.ledger_seq,
            prev_header_hash = %self.prev_header_hash.to_hex(),
            prev_ledger_seq = self.prev_header.ledger_seq,
            close_time = self.close_data.close_time,
            tx_set_hash = %self.close_data.tx_set_hash().to_hex(),
            bucket_list_hash = %bucket_list_hash.to_hex(),
            tx_result_hash = %tx_result_hash.to_hex(),
            prev_total_coins = self.prev_header.total_coins,
            total_coins_delta = self.ltx.total_coins_delta(),
            total_coins = total_coins,
            prev_fee_pool = self.prev_header.fee_pool,
            fee_pool_delta = self.ltx.fee_pool_delta(),
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
            NextHeaderFields {
                close_time: self.close_data.close_time,
                tx_set_hash: self.close_data.tx_set_hash(),
                bucket_list_hash,
                tx_set_result_hash: tx_result_hash,
                total_coins,
                fee_pool,
                inflation_seq: self.prev_header.inflation_seq,
                stellar_value_ext: self.close_data.stellar_value_ext.clone(),
            },
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

        // Also set the raw upgrades in scp_value.upgrades for correct header hash.
        // The upgrades need to be XDR-encoded as UpgradeType (opaque bytes).
        // Note: we use upgrade_ctx.upgrades (not close_data.upgrades) because
        // close_data.upgrades is drained by std::mem::take in apply_upgrades_to_delta
        // to build UpgradeEntryMeta.
        let raw_upgrades: Vec<stellar_xdr::curr::UpgradeType> = self
            .upgrade_ctx
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
        let header_xdr_hex = header_xdr_bytes.iter().fold(
            String::with_capacity(header_xdr_bytes.len() * 2),
            |mut hex, byte| {
                use std::fmt::Write as _;
                let _ = write!(hex, "{:02x}", byte);
                hex
            },
        );
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

        // Reload soroban_fee_write_1kb from post-upgrade config state.
        // The value cached at apply_transactions() time reflects pre-upgrade config;
        // config-entry upgrades may have changed rent fee parameters. The refreshed
        // value is used in LedgerCloseMetaExtV1.sorobanFeeWrite1KB.
        if protocol_version_starts_from(self.prev_header.ledger_version, ProtocolVersion::V20) {
            if let Ok(post_upgrade_config) =
                crate::execution::load_soroban_config(&self.ltx, protocol_version)
            {
                self.soroban_fee_write_1kb = post_upgrade_config.rent_fee_config.fee_per_write_1kb;
            }
        }

        tracing::debug!(
            ledger_seq = self.close_data.ledger_seq,
            delta_count_final = self.ltx.num_changes(),
            "Delta entry count before bucket list update"
        );

        // Load state archival settings BEFORE acquiring bucket list lock to avoid deadlock.
        // The snapshot's lookup_fn tries to acquire a read lock on bucket_list, which would
        // deadlock if we're already holding the write lock.
        // Parity: In stellar-core, eviction runs after config upgrades (sealLedgerTxnAndStoreInBucketsAndDB),
        // so it reads the post-upgrade StateArchival settings. CloseLedgerState's read path
        // provides this automatically.
        // Gate on prev_version (initialLedgerVers) to match stellar-core: on the upgrade
        // ledger (e.g. protocol 0→25), eviction does NOT run.
        let eviction_settings = if protocol_version_starts_from(prev_version, ProtocolVersion::V23)
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
        // Drains entries from the current delta (moving instead of cloning), saving ~50K clone operations.
        // Metadata (fee_pool_delta, total_coins_delta) is preserved for build_and_hash_header.
        let cat = self.ltx.drain_for_bucket_update();
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
        let (
            bucket_list_hash,
            bucket_lock_wait_us,
            eviction_us,
            soroban_state_us,
            add_batch_us,
            hot_archive_us,
            bg_eviction_data,
            evicted_meta_keys,
        ) = {
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

            if protocol_version_starts_from(prev_version, ProtocolVersion::V23) {
                let has_hot_archive = self.manager.hot_archive_bucket_list.read().is_some();
                if has_hot_archive {
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
                                            starting_level =
                                                eviction_settings.starting_eviction_scan_level,
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

                    // Resolution phase: apply TTL filtering + live-entry
                    // invalidation + max_entries limit.
                    // This matches stellar-core resolveBackgroundEvictionScan which:
                    // 1. Filters out entries whose TTL was modified by TXs
                    // 2. Checks for (and logs) modified live entries without TTL changes
                    // 3. Evicts up to maxEntriesToArchive entries
                    // 4. Sets iterator based on whether the limit was hit
                    //
                    // Parity: stellar-core passes `ltx.getAllKeysWithoutSealing()`
                    // which is the complete set of modified keys (data + TTL).
                    // We build the equivalent from init_entries + live_entries +
                    // dead_entries.
                    let modified_keys: std::collections::HashSet<LedgerKey> = init_entries
                        .iter()
                        .chain(live_entries.iter())
                        .map(|entry| henyey_common::entry_to_key(entry))
                        .chain(dead_entries.iter().cloned())
                        .collect();

                    let bytes_scanned = eviction_result.bytes_scanned;
                    let resolved = eviction_result
                        .resolve(eviction_settings.max_entries_to_archive, &modified_keys);

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
            // Gate on prev_version (initialLedgerVers): on the upgrade ledger, this does NOT run.
            //
            // NOTE (#1094): We always call compute_state_size_window_entry on sample
            // ledgers, even if a config upgrade already placed a resized window entry
            // in live_entries. stellar-core's maybeSnapshotSorobanStateSize runs after
            // config upgrades and performs shift+push on the (possibly resized) window.
            // The compute function reads the old window from bucket_list and applies
            // both resize and shift+push in a single pass. If a config upgrade already
            // added a resize-only entry, we replace it with the resize+shift+push result.
            if prev_version >= henyey_common::MIN_SOROBAN_PROTOCOL_VERSION {
                // Check if this is a sample ledger before computing window entry.
                // Use eviction_settings (loaded from delta at line ~4414, which
                // contains post-upgrade values) instead of bucket_list.get() which
                // returns the pre-upgrade period since add_batch hasn't run yet.
                // Parity: stellar-core reads from LedgerTxn (post-upgrade).
                // Fix for AUDIT-025 (#1099).
                let sample_period = eviction_settings
                    .as_ref()
                    .map(|s| s.live_soroban_state_size_window_sample_period)
                    .unwrap_or(64);

                // Only compute state size on sample ledgers
                let is_sample_ledger =
                    sample_period > 0 && self.close_data.ledger_seq % sample_period == 0;

                if is_sample_ledger {
                    // Use in-memory Soroban state total_size() - this is the state BEFORE
                    // this ledger's changes are applied (matching stellar-core behavior)
                    let soroban_state_size = self.manager.soroban_state.read().total_size();

                    if let Some(window_entry) = crate::execution::compute_state_size_window_entry(
                        self.close_data.ledger_seq,
                        protocol_version,
                        &bucket_list,
                        soroban_state_size,
                        sample_period,
                        eviction_settings
                            .as_ref()
                            .map(|s| s.live_soroban_state_size_window_sample_size)
                            .unwrap_or(0),
                    ) {
                        // Remove any existing window entry (e.g. from a config upgrade
                        // that resized the window) — our computed entry includes both
                        // resize and shift+push.
                        live_entries.retain(|e| {
                            !matches!(
                                &e.data,
                                LedgerEntryData::ConfigSetting(
                                    stellar_xdr::curr::ConfigSettingEntry::LiveSorobanStateSizeWindow(_)
                                )
                            )
                        });
                        tracing::info!(
                            ledger_seq = self.close_data.ledger_seq,
                            soroban_state_size = soroban_state_size,
                            "Adding state size window entry to live entries (from in-memory state)"
                        );
                        live_entries.push(window_entry);
                    }
                }
            }

            // Update in-memory Soroban state with changes from this ledger.
            // This happens AFTER computing state size window (see comment above).
            let soroban_state_start = std::time::Instant::now();
            // Gate on prev_version (initialLedgerVers): on the upgrade ledger, the
            // in-memory Soroban state update via this path does NOT run.
            if prev_version >= henyey_common::MIN_SOROBAN_PROTOCOL_VERSION {
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

            // Update hot archive and compute final bucket list hash.
            //
            // stellar-core gates addHotArchiveBatch() behind `initialLedgerVers`
            // (the protocol version BEFORE upgrades), not the upgraded version.
            // This means on the upgrade ledger (e.g., protocol 0→25), the hot
            // archive is NOT updated even though the final hash combines live
            // and hot archive hashes. The hot archive stays pristine/empty.
            //
            // For the hash combination (snapshotLedger), stellar-core uses the
            // upgraded version (currentHeader.ledgerVersion). So on the upgrade
            // ledger, it combines the live hash with the empty hot archive hash.
            let hot_archive_start = std::time::Instant::now();
            let final_hash = if protocol_version_starts_from(protocol_version, ProtocolVersion::V23)
            {
                // Gate hot archive add_batch behind prev_version (initial protocol),
                // matching stellar-core's initialLedgerVers check.
                if protocol_version_starts_from(prev_version, ProtocolVersion::V23) {
                    let mut hot_archive_guard = self.manager.hot_archive_bucket_list.write();
                    if let Some(ref mut hot_archive) = *hot_archive_guard {
                        // Advance hot archive through any skipped ledgers
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
                        hot_archive.add_batch(
                            self.close_data.ledger_seq,
                            protocol_version,
                            archived_entries,
                            std::mem::take(&mut self.hot_archive_restored_keys),
                        )?;
                    }
                } else {
                    // On the upgrade ledger (prev_version < V23, protocol_version >= V23),
                    // stellar-core does NOT call addHotArchiveBatch. But we need to
                    // advance the hot archive's ledger_seq so the NEXT ledger doesn't
                    // retroactively process pre-V23 ledgers via advance_to_ledger.
                    // stellar-core's bucket list starts fresh at the first V23+ ledger
                    // (e.g., addBatch(ledgerSeq=3, ...)) with no prior history.
                    let mut hot_archive_guard = self.manager.hot_archive_bucket_list.write();
                    if let Some(ref mut hot_archive) = *hot_archive_guard {
                        hot_archive.set_ledger_seq(self.close_data.ledger_seq);
                    }
                }

                // Combine live and hot archive hashes (using upgraded version,
                // matching stellar-core's snapshotLedger)
                let hot_archive_guard = self.manager.hot_archive_bucket_list.read();
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
                    unreachable!("HotArchiveBucketList is always initialized")
                }
            } else {
                live_hash
            };
            let hot_archive_us = hot_archive_start.elapsed().as_micros() as u64;

            // Prepare data for background eviction scan (snapshot while we hold the lock).
            // Gate on prev_version (initialLedgerVers) to match stellar-core.
            let bg_eviction_data =
                if protocol_version_starts_from(prev_version, ProtocolVersion::V23) {
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

            (
                final_hash,
                bucket_lock_wait_us,
                eviction_us,
                soroban_state_us,
                add_batch_us,
                hot_archive_us,
                bg_eviction_data,
                evicted_meta_keys,
            )
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
        self.stats.record_entry_changes(
            bucket_created_count,
            bucket_updated_count,
            bucket_deleted_count,
        );

        // Commit to manager
        let commit_close_start = std::time::Instant::now();
        self.manager.commit_close(
            offer_pool_changes,
            new_header.clone(),
            header_hash,
            delta_has_offers,
            delta_has_pool_share_trustlines,
        )?;
        let commit_close_us = commit_close_start.elapsed().as_micros() as u64;

        // If protocol upgraded to a new version in the Soroban era, rebuild
        // the module cache. Compilation artifacts are protocol-version-dependent,
        // so any Soroban-era protocol change (V25→V26, V26→V27, etc.) requires
        // a fresh cache for the NEXT ledger.
        if prev_version != protocol_version
            && protocol_version_starts_from(protocol_version, ProtocolVersion::V25)
        {
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
        // Gate on prev_version (initialLedgerVers) to match stellar-core L1693.
        let avg_soroban_state_size = if prev_version >= henyey_common::MIN_SOROBAN_PROTOCOL_VERSION
        {
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
        let meta = build_ledger_close_meta(LedgerCloseMetaInputs {
            tx_set_variant: tx_set,
            scp_history,
            header: new_header.clone(),
            header_hash,
            tx_result_metas: std::mem::take(&mut self.tx_result_metas),
            evicted_keys: evicted_meta_keys,
            total_byte_size_of_live_soroban_state: avg_soroban_state_size,
            upgrades_processing: upgrades_meta,
            emit_ext_v1: self.manager.config.emit_ledger_close_meta_ext_v1,
            soroban_fee_write_1kb: self.soroban_fee_write_1kb,
        });
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
        tx_timings.sort_by_key(|a| std::cmp::Reverse(a.exec_us));

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

struct LedgerCloseMetaInputs {
    tx_set_variant: TransactionSetVariant,
    scp_history: Vec<ScpHistoryEntry>,
    header: LedgerHeader,
    header_hash: Hash256,
    tx_result_metas: Vec<TransactionResultMetaV1>,
    evicted_keys: Vec<LedgerKey>,
    total_byte_size_of_live_soroban_state: u64,
    upgrades_processing: Vec<UpgradeEntryMeta>,
    emit_ext_v1: bool,
    soroban_fee_write_1kb: i64,
}

fn build_ledger_close_meta(inputs: LedgerCloseMetaInputs) -> LedgerCloseMeta {
    let LedgerCloseMetaInputs {
        tx_set_variant,
        scp_history,
        header,
        header_hash,
        tx_result_metas,
        evicted_keys,
        total_byte_size_of_live_soroban_state,
        upgrades_processing,
        emit_ext_v1,
        soroban_fee_write_1kb,
    } = inputs;
    let ledger_header = LedgerHeaderHistoryEntry {
        hash: Hash::from(header_hash),
        header,
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
    use crate::delta::LedgerDelta;
    use stellar_xdr::curr::{
        Asset, ContractDataDurability, ContractDataEntry, ContractId, ExtensionPoint,
        LedgerScpMessages, OfferEntry, OfferEntryExt, Price, ScAddress, ScVal, ScpHistoryEntry,
        ScpHistoryEntryV0, TransactionSet, TtlEntry, WriteXdr,
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
        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
        assert!(result.offers.is_empty());
        assert!(result.soroban_state.is_empty());
    }

    #[test]
    fn test_scan_offers_from_bucket_list() {
        let mut bl = BucketList::new();
        let offer1 = make_offer_entry(1, [1u8; 32]);
        let offer2 = make_offer_entry(2, [2u8; 32]);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![offer1, offer2],
            vec![],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
        assert_eq!(result.offers.len(), 2);
        assert!(result.offers.contains_key(&1));
        assert!(result.offers.contains_key(&2));
    }

    #[test]
    fn test_scan_contract_data_from_bucket_list() {
        let mut bl = BucketList::new();
        let cd1 = make_contract_data_entry([10u8; 32]);
        let cd2 = make_contract_data_entry([20u8; 32]);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![cd1, cd2],
            vec![],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
        assert_eq!(result.soroban_state.contract_data_count(), 2);
    }

    #[test]
    fn test_scan_ttl_entries_from_bucket_list() {
        let mut bl = BucketList::new();
        let ttl = make_ttl_entry([30u8; 32], 1000);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![ttl],
            vec![],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
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
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![offer, cd, ttl],
            vec![],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
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
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![offer],
            vec![],
            vec![],
        )
        .unwrap();

        let dead_key = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: make_account_id([5u8; 32]),
            offer_id: 99,
        });
        bl.add_batch(
            2,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![],
            vec![dead_key],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
        assert!(
            result.offers.is_empty(),
            "dead entry should shadow the live offer"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_scan_newer_level_shadows_older() {
        // Add entries at different ledgers so they end up at different levels.
        // Lower-numbered levels (newer data) should shadow higher ones.
        let mut bl = BucketList::new();

        // Add offer at ledger 1 (will be in a higher level after more adds)
        let old_offer = make_offer_entry(1, [1u8; 32]);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![old_offer],
            vec![],
            vec![],
        )
        .unwrap();

        // Modify the same offer at ledger 2 (more recent → lower level)
        let mut new_offer = make_offer_entry(1, [1u8; 32]);
        if let LedgerEntryData::Offer(ref mut o) = new_offer.data {
            o.amount = 9999;
        }
        bl.add_batch(
            2,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![new_offer.clone()],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
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
                (i + 1) as u32,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![offer],
                vec![],
                vec![],
            )
            .unwrap();
        }

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
        assert_eq!(
            result.offers.len(),
            num_offers,
            "all offers should be found"
        );
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
            1,
            pre_soroban_protocol,
            BucketListType::Live,
            vec![offer, cd],
            vec![],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, pre_soroban_protocol, 2).unwrap();
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
        let result = scan_single_level(&curr, &snap, true, &mc, TEST_PROTOCOL).unwrap();

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
        let result = scan_single_level(&curr, &snap, true, &mc, TEST_PROTOCOL).unwrap();

        // The dead entry shadows the live one → no entries in result
        assert!(
            result.entries.is_empty(),
            "dead entry should shadow live entry in snap"
        );
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

        let result = merge_level_results(vec![level0, level1], None, TEST_PROTOCOL, &None);

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
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![offer],
            vec![],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
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
                (i + 1) as u32,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![offer],
                vec![],
                vec![],
            )
            .unwrap();
        }

        let result1 = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        let result2 = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();

        assert_eq!(result1.offers.len(), result2.offers.len());
        for (id, entry) in &result1.offers {
            assert!(
                result2.offers.contains_key(id),
                "parallel result missing offer {id}"
            );
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
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![offer],
            vec![],
            vec![],
        )
        .unwrap();

        let dead_key = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: make_account_id([7u8; 32]),
            offer_id: 77,
        });
        bl.add_batch(
            2,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![],
            vec![dead_key],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        assert!(
            result.offers.is_empty(),
            "dead entry should shadow live offer (thread_count=1)"
        );
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
        let expected = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();

        // Extract pairs and scan via scan_level_pairs_for_caches
        let level_pairs: Vec<(Arc<henyey_bucket::Bucket>, Arc<henyey_bucket::Bucket>)> = bl
            .levels()
            .iter()
            .map(|l| (l.curr.clone(), l.snap.clone()))
            .collect();
        let actual = scan_level_pairs_for_caches(level_pairs, TEST_PROTOCOL, 2).unwrap();

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

    // ---- Streaming scan-and-merge tests (scan_thread_count=1) ----

    #[test]
    fn test_streaming_scan_empty_bucket_list() {
        let bl = BucketList::new();
        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        assert!(result.offers.is_empty());
        assert!(result.soroban_state.is_empty());
    }

    #[test]
    fn test_streaming_scan_offers() {
        let mut bl = BucketList::new();
        let offer1 = make_offer_entry(1, [1u8; 32]);
        let offer2 = make_offer_entry(2, [2u8; 32]);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![offer1, offer2],
            vec![],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        assert_eq!(result.offers.len(), 2);
        assert!(result.offers.contains_key(&1));
        assert!(result.offers.contains_key(&2));
    }

    #[test]
    fn test_streaming_scan_contract_data() {
        let mut bl = BucketList::new();
        let cd1 = make_contract_data_entry([10u8; 32]);
        let cd2 = make_contract_data_entry([20u8; 32]);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![cd1, cd2],
            vec![],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        assert_eq!(result.soroban_state.contract_data_count(), 2);
    }

    #[test]
    fn test_streaming_scan_ttl_entries() {
        let mut bl = BucketList::new();
        // Add contract data entries with corresponding TTLs
        let cd1 = make_contract_data_entry([30u8; 32]);
        let cd2 = make_contract_data_entry([31u8; 32]);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![cd1, cd2],
            vec![],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        assert_eq!(
            result.soroban_state.contract_data_count(),
            2,
            "should have 2 contract data entries"
        );
    }

    #[test]
    fn test_streaming_scan_mixed_types() {
        let mut bl = BucketList::new();
        let offer = make_offer_entry(1, [1u8; 32]);
        let cd = make_contract_data_entry([10u8; 32]);
        let ttl = make_ttl_entry([30u8; 32], 1000);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![offer, cd, ttl],
            vec![],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        assert_eq!(result.offers.len(), 1);
        assert_eq!(result.soroban_state.contract_data_count(), 1);
        assert!(
            !result.soroban_state.is_empty(),
            "soroban state should contain the TTL entry"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_streaming_dead_entry_shadows_live() {
        let mut bl = BucketList::new();
        // Add an offer at ledger 1
        let offer = make_offer_entry(1, [1u8; 32]);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![offer.clone()],
            vec![],
            vec![],
        )
        .unwrap();
        // Kill it at ledger 2
        let dead_key = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: make_account_id([1u8; 32]),
            offer_id: 1,
        });
        bl.add_batch(
            2,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![],
            vec![dead_key],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        assert!(
            result.offers.is_empty(),
            "dead entry should shadow the live offer"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_streaming_newer_level_shadows_older() {
        let mut bl = BucketList::new();
        // Add offer at ledger 1 (will end up at a higher/older level after merges)
        let offer_v1 = make_offer_entry(1, [1u8; 32]);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![offer_v1],
            vec![],
            vec![],
        )
        .unwrap();
        // Modify same offer (same key) at later ledger with different amount
        let mut offer_v2 = make_offer_entry(1, [1u8; 32]);
        if let LedgerEntryData::Offer(ref mut o) = offer_v2.data {
            o.amount = 9999;
        }
        bl.add_batch(
            2,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![offer_v2],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        assert_eq!(result.offers.len(), 1);
        // The newer version (amount=9999) should win
        if let LedgerEntryData::Offer(ref o) = result.offers[&1].data {
            assert_eq!(o.amount, 9999, "newer level should shadow older level");
        } else {
            panic!("expected offer entry");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_streaming_many_entries_across_levels() {
        let mut bl = BucketList::new();
        for i in 1u32..=20 {
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

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        assert_eq!(result.offers.len(), 20);
    }

    #[test]
    fn test_streaming_pre_soroban_protocol() {
        let mut bl = BucketList::new();
        let pre_soroban_protocol = 19; // Before Soroban was enabled
        let offer = make_offer_entry(1, [1u8; 32]);
        let cd = make_contract_data_entry([10u8; 32]);
        bl.add_batch(
            1,
            pre_soroban_protocol,
            BucketListType::Live,
            vec![offer, cd],
            vec![],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, pre_soroban_protocol, 1).unwrap();
        // Offers should still be scanned even without Soroban
        assert_eq!(result.offers.len(), 1);
        // Contract data should not be scanned when Soroban is disabled
        assert_eq!(result.soroban_state.contract_data_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_streaming_intra_level_dedup() {
        // Within a single level, curr shadows snap. If the same key appears
        // in both curr and snap, only the curr version should be kept.
        let mut bl = BucketList::new();
        // Add offer at ledger 1 (goes into snap after ledger 2 closes)
        let offer_v1 = make_offer_entry(1, [1u8; 32]);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![offer_v1],
            vec![],
            vec![],
        )
        .unwrap();
        // Modify same offer at ledger 2 (same key, different amount) — goes into curr
        let mut offer_v2 = make_offer_entry(1, [1u8; 32]);
        if let LedgerEntryData::Offer(ref mut o) = offer_v2.data {
            o.amount = 7777;
        }
        bl.add_batch(
            2,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![offer_v2],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        assert_eq!(result.offers.len(), 1, "should have exactly one offer");
        if let LedgerEntryData::Offer(ref o) = result.offers[&1].data {
            assert_eq!(
                o.amount, 7777,
                "curr should shadow snap within the same level"
            );
        } else {
            panic!("expected offer entry");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_streaming_cross_level_dedup() {
        // If the same key appears in different levels, the lower-numbered
        // (newer) level should win.
        let mut bl = BucketList::new();
        // Create offer at ledger 1
        let offer_v1 = make_offer_entry(1, [1u8; 32]);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![offer_v1],
            vec![],
            vec![],
        )
        .unwrap();
        // Update same offer across several more ledgers to push data to higher levels
        for i in 2u32..=8 {
            let mut offer = make_offer_entry(1, [1u8; 32]);
            if let LedgerEntryData::Offer(ref mut o) = offer.data {
                o.amount = i as i64 * 1000;
            }
            bl.add_batch(
                i,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![],
                vec![offer],
                vec![],
            )
            .unwrap();
        }

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        // Should have exactly 1 offer (all were for offer_id=1)
        assert_eq!(result.offers.len(), 1);
        // The newest version (amount=8000) should win
        if let LedgerEntryData::Offer(ref o) = result.offers[&1].data {
            assert_eq!(
                o.amount, 8000,
                "most recent version should win across levels"
            );
        } else {
            panic!("expected offer entry");
        }
    }

    #[test]
    fn test_streaming_pool_share_trustline_indexing() {
        let mut bl = BucketList::new();
        let pool_hash = stellar_xdr::curr::PoolId(Hash([99u8; 32]));
        let account = make_account_id([1u8; 32]);
        let tl_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::Trustline(stellar_xdr::curr::TrustLineEntry {
                account_id: account.clone(),
                asset: stellar_xdr::curr::TrustLineAsset::PoolShare(pool_hash.clone()),
                balance: 100,
                limit: 1000,
                flags: 0,
                ext: stellar_xdr::curr::TrustLineEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![tl_entry],
            vec![],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        assert!(
            result
                .pool_share_tl_account_index
                .get(&account)
                .map_or(false, |pools| pools.contains(&pool_hash)),
            "pool share trustline should be indexed"
        );
    }

    #[test]
    fn test_streaming_config_settings() {
        let mut bl = BucketList::new();
        let config_entry = LedgerEntry {
            last_modified_ledger_seq: 1,
            data: LedgerEntryData::ConfigSetting(
                stellar_xdr::curr::ConfigSettingEntry::ContractMaxSizeBytes(16384),
            ),
            ext: LedgerEntryExt::V0,
        };
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![config_entry],
            vec![],
            vec![],
        )
        .unwrap();

        let result = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        // Config settings are loaded into soroban_state
        assert!(
            !result.soroban_state.is_empty(),
            "config settings should be loaded into soroban state"
        );
    }

    // ---- Streaming vs parallel equivalence tests ----

    /// Helper: assert two `CacheInitResult`s are equivalent.
    fn assert_cache_init_results_equivalent(
        streaming: &CacheInitResult,
        parallel: &CacheInitResult,
        label: &str,
    ) {
        assert_eq!(
            streaming.offers.len(),
            parallel.offers.len(),
            "{}: offer count mismatch",
            label
        );
        for (id, _) in &parallel.offers {
            assert!(
                streaming.offers.contains_key(id),
                "{}: offer {} missing from streaming result",
                label,
                id
            );
        }
        assert_eq!(
            streaming.pool_share_tl_account_index.len(),
            parallel.pool_share_tl_account_index.len(),
            "{}: pool share index count mismatch",
            label
        );
        assert_eq!(
            streaming.soroban_state.contract_data_count(),
            parallel.soroban_state.contract_data_count(),
            "{}: contract data count mismatch",
            label
        );
        assert_eq!(
            streaming.soroban_state.contract_code_count(),
            parallel.soroban_state.contract_code_count(),
            "{}: contract code count mismatch",
            label
        );
    }

    #[test]
    fn test_streaming_vs_parallel_empty() {
        let bl = BucketList::new();
        let streaming = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        let parallel = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
        assert_cache_init_results_equivalent(&streaming, &parallel, "empty");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_streaming_vs_parallel_offers_only() {
        let mut bl = BucketList::new();
        for i in 1i64..=5 {
            let offer = make_offer_entry(i, [i as u8; 32]);
            bl.add_batch(
                i as u32,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![offer],
                vec![],
                vec![],
            )
            .unwrap();
        }
        let streaming = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        let parallel = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
        assert_cache_init_results_equivalent(&streaming, &parallel, "offers_only");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_streaming_vs_parallel_mixed_types() {
        let mut bl = BucketList::new();
        for i in 1u32..=8 {
            let offer = make_offer_entry(i as i64, [i as u8; 32]);
            let cd = make_contract_data_entry([(i + 100) as u8; 32]);
            let ttl = make_ttl_entry([(i + 200) as u8; 32], i * 1000);
            bl.add_batch(
                i,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![offer, cd, ttl],
                vec![],
                vec![],
            )
            .unwrap();
        }
        let streaming = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        let parallel = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
        assert_cache_init_results_equivalent(&streaming, &parallel, "mixed_types");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_streaming_vs_parallel_with_dead_entries() {
        let mut bl = BucketList::new();
        // Add offers
        for i in 1i64..=5 {
            let offer = make_offer_entry(i, [i as u8; 32]);
            bl.add_batch(
                i as u32,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![offer],
                vec![],
                vec![],
            )
            .unwrap();
        }
        // Kill offers 2 and 4
        let dead2 = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: make_account_id([2u8; 32]),
            offer_id: 2,
        });
        let dead4 = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: make_account_id([4u8; 32]),
            offer_id: 4,
        });
        bl.add_batch(
            6,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![],
            vec![],
            vec![dead2, dead4],
        )
        .unwrap();
        let streaming = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        let parallel = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
        assert_cache_init_results_equivalent(&streaming, &parallel, "with_dead_entries");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_streaming_vs_parallel_cross_level_overwrites() {
        let mut bl = BucketList::new();
        // Create offer and contract data at ledger 1
        let offer_init = make_offer_entry(1, [1u8; 32]);
        let cd_init = make_contract_data_entry([1u8; 32]);
        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            vec![offer_init, cd_init],
            vec![],
            vec![],
        )
        .unwrap();
        // Repeatedly overwrite the same entries to create cross-level duplicates
        for i in 2u32..=16 {
            let mut offer = make_offer_entry(1, [1u8; 32]);
            if let LedgerEntryData::Offer(ref mut o) = offer.data {
                o.amount = i as i64 * 100;
            }
            let cd = make_contract_data_entry([1u8; 32]);
            bl.add_batch(
                i,
                TEST_PROTOCOL,
                BucketListType::Live,
                vec![],
                vec![offer, cd],
                vec![],
            )
            .unwrap();
        }
        let streaming = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 1).unwrap();
        let parallel = scan_bucket_list_for_caches(&bl, TEST_PROTOCOL, 2).unwrap();
        assert_cache_init_results_equivalent(&streaming, &parallel, "cross_level_overwrites");
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
        let manager = LedgerManager::new("Test SDF Network ; September 2015".to_string(), config);

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
        let manager = LedgerManager::new("Test SDF Network ; September 2015".to_string(), config);

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
            manager
                .bucket_list
                .read()
                .bucket_list_db_config()
                .unwrap()
                .memory_for_caching_mb,
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
            manager
                .bucket_list
                .read()
                .bucket_list_db_config()
                .unwrap()
                .memory_for_caching_mb,
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
        let meta = build_ledger_close_meta(LedgerCloseMetaInputs {
            tx_set_variant: close_data.tx_set,
            scp_history: close_data.scp_history,
            header,
            header_hash: Hash256::ZERO,
            tx_result_metas: Vec::new(),
            evicted_keys: Vec::new(),
            total_byte_size_of_live_soroban_state: 0,
            upgrades_processing: Vec::new(),
            emit_ext_v1: false,
            soroban_fee_write_1kb: 0,
        });
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
        let snapshot = BucketListSnapshot::new(&BucketList::default(), create_genesis_header());
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
                data: LedgerEntryData::ContractCode(stellar_xdr::curr::ContractCodeEntry {
                    ext: stellar_xdr::curr::ContractCodeEntryExt::V0,
                    hash: Hash(hash_bytes),
                    code: vec![0u8; 50].try_into().unwrap(),
                }),
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

        bl.add_batch(
            1,
            TEST_PROTOCOL,
            BucketListType::Live,
            entries,
            vec![],
            vec![],
        )
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

        let handle =
            std::thread::spawn(move || snapshot.scan_for_eviction_incremental(iter, 5, &settings));

        let result = handle.join().expect("thread should not panic").unwrap();
        assert_eq!(result.candidates.len(), 3, "Should find 3 expired entries");
        assert!(result.bytes_scanned > 0);
    }

    // ---- Genesis createLedgerEntries tests ----

    /// Helper to create a minimal `LedgerCloseContext` for testing genesis entry creation.
    ///
    /// The returned context has an empty snapshot and delta at the given ledger_seq.
    /// The manager is initialized with an empty bucket list.
    fn make_test_close_context(manager: &LedgerManager, ledger_seq: u32) -> LedgerCloseContext<'_> {
        let header = create_genesis_header();
        let header_hash = crate::compute_header_hash(&header).expect("hash");
        let snapshot = SnapshotHandle::new(crate::snapshot::LedgerSnapshot::empty(0));

        let ltx = CloseLedgerState::begin(snapshot, header.clone(), header_hash, ledger_seq);

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
            ltx,
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
        let entry = get_config_setting_from_delta(
            ctx.ltx.current_delta(),
            ConfigSettingId::ContractMaxSizeBytes,
        );
        assert!(entry.is_some(), "ContractMaxSizeBytes should exist");
        if let Some(ConfigSettingEntry::ContractMaxSizeBytes(v)) = entry {
            assert_eq!(v, 2_000);
        } else {
            panic!("Wrong type for ContractMaxSizeBytes");
        }

        // 2. ContractDataKeySizeBytes
        let entry = get_config_setting_from_delta(
            ctx.ltx.current_delta(),
            ConfigSettingId::ContractDataKeySizeBytes,
        );
        assert!(entry.is_some(), "ContractDataKeySizeBytes should exist");

        // 3. ContractDataEntrySizeBytes
        let entry = get_config_setting_from_delta(
            ctx.ltx.current_delta(),
            ConfigSettingId::ContractDataEntrySizeBytes,
        );
        assert!(entry.is_some(), "ContractDataEntrySizeBytes should exist");

        // 4. ContractComputeV0
        let entry = get_config_setting_from_delta(
            ctx.ltx.current_delta(),
            ConfigSettingId::ContractComputeV0,
        );
        assert!(entry.is_some(), "ContractComputeV0 should exist");
        if let Some(ConfigSettingEntry::ContractComputeV0(ref compute)) = entry {
            assert_eq!(compute.tx_max_instructions, 2_500_000);
            assert_eq!(compute.tx_memory_limit, 2_000_000);
        }

        // 5. ContractLedgerCostV0
        let entry = get_config_setting_from_delta(
            ctx.ltx.current_delta(),
            ConfigSettingId::ContractLedgerCostV0,
        );
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
                ctx.ltx.current_delta(),
                ConfigSettingId::ContractHistoricalDataV0
            )
            .is_some(),
            "ContractHistoricalDataV0 should exist"
        );
        assert!(
            get_config_setting_from_delta(
                ctx.ltx.current_delta(),
                ConfigSettingId::ContractEventsV0
            )
            .is_some(),
            "ContractEventsV0 should exist"
        );
        assert!(
            get_config_setting_from_delta(
                ctx.ltx.current_delta(),
                ConfigSettingId::ContractBandwidthV0
            )
            .is_some(),
            "ContractBandwidthV0 should exist"
        );
        assert!(
            get_config_setting_from_delta(
                ctx.ltx.current_delta(),
                ConfigSettingId::ContractExecutionLanes
            )
            .is_some(),
            "ContractExecutionLanes should exist"
        );

        // 10-11. CPU and memory cost params (23 entries each)
        let cpu = get_config_setting_from_delta(
            ctx.ltx.current_delta(),
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
            ctx.ltx.current_delta(),
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
            get_config_setting_from_delta(ctx.ltx.current_delta(), ConfigSettingId::StateArchival);
        assert!(archival.is_some(), "StateArchival should exist");
        if let Some(ConfigSettingEntry::StateArchival(ref sa)) = archival {
            assert_eq!(sa.max_entry_ttl, 1_054_080);
            assert_eq!(sa.min_temporary_ttl, 16);
            assert_eq!(sa.min_persistent_ttl, 4_096);
            assert_eq!(sa.starting_eviction_scan_level, 6);
        }

        // 13. LiveSorobanStateSizeWindow (30-entry window)
        let window = get_config_setting_from_delta(
            ctx.ltx.current_delta(),
            ConfigSettingId::LiveSorobanStateSizeWindow,
        );
        assert!(window.is_some(), "LiveSorobanStateSizeWindow should exist");
        if let Some(ConfigSettingEntry::LiveSorobanStateSizeWindow(ref w)) = window {
            assert_eq!(w.len(), 30, "Window should have 30 entries");
        }

        // 14. EvictionIterator
        let eviction = get_config_setting_from_delta(
            ctx.ltx.current_delta(),
            ConfigSettingId::EvictionIterator,
        );
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
            ctx.ltx.current_delta(),
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
            ctx.ltx.current_delta(),
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
            ctx.ltx.current_delta(),
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
            ctx.ltx.current_delta(),
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
            ctx.ltx.current_delta(),
            ConfigSettingId::ContractParallelComputeV0,
        );
        assert!(parallel.is_some(), "ContractParallelComputeV0 should exist");
        if let Some(ConfigSettingEntry::ContractParallelComputeV0(ref p)) = parallel {
            assert_eq!(p.ledger_max_dependent_tx_clusters, 1);
        }

        // 2. ScpTiming
        let timing =
            get_config_setting_from_delta(ctx.ltx.current_delta(), ConfigSettingId::ScpTiming);
        assert!(timing.is_some(), "ScpTiming should exist");
        if let Some(ConfigSettingEntry::ScpTiming(ref t)) = timing {
            assert_eq!(t.ledger_target_close_time_milliseconds, 5000);
            assert_eq!(t.nomination_timeout_initial_milliseconds, 1000);
        }

        // 3. ContractLedgerCostExtV0
        let ext = get_config_setting_from_delta(
            ctx.ltx.current_delta(),
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
            ctx.ltx.current_delta(),
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
            get_config_setting_from_delta(ctx.ltx.current_delta(), ConfigSettingId::StateArchival);
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
            ctx.ltx.current_delta(),
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
            ctx.ltx.current_delta(),
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
    fn test_full_chain_v20_through_v26_cost_params() {
        // Test the full cost params chain including V26 updates
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

        // Full chain: V20 -> V21 -> V22 -> V23 -> V25 -> V26
        ctx.create_ledger_entries_for_v20().expect("V20");
        ctx.create_cost_types_for_v21().expect("V21");
        ctx.create_cost_types_for_v22().expect("V22");
        ctx.create_and_update_ledger_entries_for_v23().expect("V23");
        ctx.create_cost_types_for_v25().expect("V25");
        ctx.update_cost_types_for_v26().expect("V26");

        // CPU cost params should have 86 entries (Bn254G1Msm at index 85)
        let cpu = get_config_setting_from_delta(
            ctx.ltx.current_delta(),
            ConfigSettingId::ContractCostParamsCpuInstructions,
        );
        if let Some(ConfigSettingEntry::ContractCostParamsCpuInstructions(ref params)) = cpu {
            assert_eq!(
                params.0.len(),
                86,
                "V26 CPU cost params should have 86 entries"
            );
            // Verify V26-updated BLS12-381 values
            assert_eq!(params.0[55].const_term, 2347584); // Bls12381G1Msm
            assert_eq!(params.0[55].linear_term, 94135478);
            assert_eq!(params.0[56].const_term, 1020885); // Bls12381MapFpToG1
            assert_eq!(params.0[57].const_term, 2638451); // Bls12381HashToG1
            assert_eq!(params.0[57].linear_term, 6803);
            assert_eq!(params.0[60].const_term, 7663880); // Bls12381G2Msm
            assert_eq!(params.0[60].linear_term, 298580871);
            assert_eq!(params.0[61].const_term, 1856539); // Bls12381MapFp2ToG2
            assert_eq!(params.0[62].const_term, 6315452); // Bls12381HashToG2
            assert_eq!(params.0[62].linear_term, 7232);
            // Verify V26-updated BN254 value
            assert_eq!(params.0[74].const_term, 1706052); // Bn254G2CheckPointInSubgroup
                                                          // Verify new Bn254G1Msm entry
            assert_eq!(params.0[85].const_term, 1185193); // Bn254G1Msm
            assert_eq!(params.0[85].linear_term, 41568084);
            // Verify older entries are preserved
            assert_eq!(params.0[0].const_term, 4); // WasmInsnExec (V20)
            assert_eq!(params.0[84].const_term, 33151); // Bn254FrInv (V25)
        } else {
            panic!("CPU cost params not found after V26 upgrade");
        }

        // Memory cost params should also have 86 entries
        let mem = get_config_setting_from_delta(
            ctx.ltx.current_delta(),
            ConfigSettingId::ContractCostParamsMemoryBytes,
        );
        if let Some(ConfigSettingEntry::ContractCostParamsMemoryBytes(ref params)) = mem {
            assert_eq!(
                params.0.len(),
                86,
                "V26 memory cost params should have 86 entries"
            );
            // Verify V26-updated memory values
            assert_eq!(params.0[55].const_term, 109494); // Bls12381G1Msm
            assert_eq!(params.0[55].linear_term, 266603);
            assert_eq!(params.0[56].const_term, 2776); // Bls12381MapFpToG1
            assert_eq!(params.0[57].const_term, 5896); // Bls12381HashToG1
            assert_eq!(params.0[60].const_term, 219654); // Bls12381G2Msm
            assert_eq!(params.0[60].linear_term, 266603);
            assert_eq!(params.0[61].const_term, 1672); // Bls12381MapFp2ToG2
            assert_eq!(params.0[62].const_term, 3960); // Bls12381HashToG2
                                                       // Verify new Bn254G1Msm entry
            assert_eq!(params.0[85].const_term, 73061); // Bn254G1Msm
            assert_eq!(params.0[85].linear_term, 229779);
        } else {
            panic!("Memory cost params not found after V26 upgrade");
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

    /// Regression test: build_and_hash_header must encode upgrades from upgrade_ctx,
    /// not from close_data.upgrades (which is drained by std::mem::take in
    /// apply_upgrades_to_delta before build_and_hash_header runs).
    #[test]
    fn test_header_scp_value_upgrades_populated_after_drain() {
        use stellar_xdr::curr::{LedgerUpgrade, Limits, ReadXdr};

        let manager = LedgerManager::new(
            "Test SDF Network ; September 2015".to_string(),
            LedgerManagerConfig {
                validate_bucket_hash: false,
                ..Default::default()
            },
        );

        let mut ctx = make_test_close_context(&manager, 2);

        // Simulate what begin_close does: add upgrades to both close_data and upgrade_ctx.
        let upgrades = vec![
            LedgerUpgrade::Version(25),
            LedgerUpgrade::BaseReserve(5_000_000),
        ];
        ctx.close_data.upgrades = upgrades.clone();
        for u in &upgrades {
            ctx.upgrade_ctx.add_upgrade(u.clone());
        }

        // Simulate what apply_upgrades_to_delta does: drain close_data.upgrades.
        let _drained = std::mem::take(&mut ctx.close_data.upgrades);
        assert!(
            ctx.close_data.upgrades.is_empty(),
            "upgrades should be drained"
        );
        assert_eq!(
            ctx.upgrade_ctx.upgrades.len(),
            2,
            "upgrade_ctx should still have upgrades"
        );

        // build_and_hash_header should use upgrade_ctx.upgrades, not close_data.upgrades.
        let (header, _hash) = ctx
            .build_and_hash_header(Hash256::ZERO, Hash256::ZERO, false, false)
            .expect("build_and_hash_header should succeed");

        // Verify scp_value.upgrades is populated (not empty).
        assert_eq!(
            header.scp_value.upgrades.len(),
            2,
            "scp_value.upgrades must contain the 2 upgrades (version + base_reserve)"
        );

        // Verify the actual upgrade content.
        let decoded: Vec<LedgerUpgrade> = header
            .scp_value
            .upgrades
            .iter()
            .map(|u| LedgerUpgrade::from_xdr(&u.0, Limits::none()).unwrap())
            .collect();
        assert!(matches!(decoded[0], LedgerUpgrade::Version(25)));
        assert!(matches!(decoded[1], LedgerUpgrade::BaseReserve(5_000_000)));
    }

    /// Regression test: bucket_list() read guard must be dropped before
    /// bucket_list_mut() write guard is acquired on the same RwLock.
    /// Holding both simultaneously deadlocks (parking_lot RwLock is not
    /// reentrant). This pattern caused the post-catchup HAS persist to
    /// deadlock, preventing the node from transitioning to real-time
    /// consensus.
    #[test]
    fn test_bucket_list_read_then_write_no_deadlock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let bucket_list: parking_lot::RwLock<BucketList> =
            parking_lot::RwLock::new(BucketList::new());

        // Correct pattern: read, extract, drop, then write.
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let hash = {
                let guard = bucket_list.read();
                let h = guard.hash();
                drop(guard);
                h
            };
            // Write lock should succeed immediately after read is dropped.
            let _write_guard = bucket_list.write();
            let _ = tx.send(hash);
        });

        // If the thread doesn't finish within 2 seconds, it's deadlocked.
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(_hash) => {} // Success — no deadlock
            Err(_) => panic!(
                "Deadlock detected: bucket_list read guard was not \
                 dropped before write guard acquisition"
            ),
        }
        handle.join().unwrap();
    }

    #[test]
    fn test_soroban_network_info_returns_none_before_initialization() {
        let mgr = LedgerManager::new(
            "Test SDF Network ; September 2015".into(),
            LedgerManagerConfig::default(),
        );
        // Before initialization, soroban_network_info should return None.
        assert!(mgr.soroban_network_info().is_none());
    }

    #[test]
    fn test_soroban_network_info_cleared_on_reset() {
        let mgr = LedgerManager::new(
            "Test SDF Network ; September 2015".into(),
            LedgerManagerConfig::default(),
        );
        // Manually inject a cached value to verify reset clears it.
        {
            let mut state = mgr.state.write();
            state.soroban_network_info = Some(SorobanNetworkInfo::default());
        }
        assert!(mgr.soroban_network_info().is_some());

        mgr.reset();
        assert!(mgr.soroban_network_info().is_none());
    }

    #[test]
    fn test_soroban_network_info_accessor_is_o1() {
        // Verify the accessor reads from state without creating a snapshot.
        // We inject a value and confirm repeated reads return it identically.
        let mgr = LedgerManager::new(
            "Test SDF Network ; September 2015".into(),
            LedgerManagerConfig::default(),
        );
        let mut info = SorobanNetworkInfo::default();
        info.tx_max_instructions = 42;
        info.ledger_max_tx_count = 99;
        {
            let mut state = mgr.state.write();
            state.soroban_network_info = Some(info.clone());
        }

        // Multiple reads should return the same cached value.
        let r1 = mgr.soroban_network_info();
        let r2 = mgr.soroban_network_info();
        assert_eq!(r1.unwrap().tx_max_instructions, 42);
        assert_eq!(r2.unwrap().ledger_max_tx_count, 99);
    }
}
