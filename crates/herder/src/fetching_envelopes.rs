//! Fetching envelopes management.
//!
//! This module handles SCP envelopes that are waiting for their dependencies
//! (TxSets and QuorumSets) to be fetched from peers. When an envelope arrives
//! referencing data we don't have, we start fetching it and queue the envelope.
//! Once all dependencies are received, the envelope is ready for processing.
//!
//! This is the Rust equivalent of stellar-core's `PendingEnvelopes` fetching logic.

use dashmap::DashMap;
use henyey_common::Hash256;
use henyey_overlay::{ItemFetcher, ItemFetcherConfig, ItemType, PeerId};
use henyey_scp::{is_quorum_set_sane, SlotIndex};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use stellar_xdr::curr::{Hash, Limits, ReadXdr, ScpEnvelope, ScpQuorumSet};
use tracing::{debug, trace};

/// Callback type for requesting items from peers.
type AskPeerFn = Box<dyn Fn(&PeerId, &Hash, ItemType) + Send + Sync>;

/// Callback type for broadcasting ready envelopes to peers.
type BroadcastFn = Box<dyn Fn(&ScpEnvelope) + Send + Sync>;

/// Result of receiving an SCP envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecvResult {
    /// Envelope is ready for processing (all dependencies available).
    Ready,
    /// Envelope is being fetched (waiting for dependencies).
    Fetching,
    /// Envelope was already processed or is a duplicate.
    AlreadyProcessed,
    /// Envelope was discarded (invalid or rejected).
    Discarded,
}

/// State of envelopes for a single slot.
#[derive(Default)]
pub struct SlotEnvelopes {
    /// Envelopes we have discarded.
    discarded: HashSet<Hash256>,
    /// Envelopes we have processed already.
    processed: HashSet<Hash256>,
    /// Envelopes we are fetching right now (hash -> received time).
    fetching: HashMap<Hash256, (ScpEnvelope, Instant)>,
    /// Envelopes that are ready to be processed.
    ready: Vec<ScpEnvelope>,
}

/// Configuration for fetching envelopes.
#[derive(Debug, Clone)]
pub struct FetchingConfig {
    /// Configuration for the TxSet fetcher.
    pub tx_set_fetcher_config: ItemFetcherConfig,
    /// Configuration for the QuorumSet fetcher.
    pub quorum_set_fetcher_config: ItemFetcherConfig,
    /// Maximum slots to track.
    pub max_slots: usize,
    /// Maximum tx sets to cache. Once exceeded, an arbitrary entry is evicted.
    pub max_tx_set_cache: usize,
    /// Maximum quorum sets to cache. Once exceeded, an arbitrary entry is evicted.
    pub max_quorum_set_cache: usize,
}

impl Default for FetchingConfig {
    fn default() -> Self {
        Self {
            tx_set_fetcher_config: ItemFetcherConfig::default(),
            quorum_set_fetcher_config: ItemFetcherConfig::default(),
            max_slots: 12,
            // Parity: stellar-core uses TXSET_CACHE_SIZE = 10000 and
            // QSET_CACHE_SIZE = 10000 in PendingEnvelopes.cpp.
            max_tx_set_cache: 10_000,
            max_quorum_set_cache: 10_000,
        }
    }
}

/// Manages fetching of TxSets and QuorumSets for SCP envelopes.
///
/// When an SCP envelope references a TxSet or QuorumSet we don't have,
/// this manager starts fetching it from peers. Once received, envelopes
/// waiting for that data become ready for processing.
pub struct FetchingEnvelopes {
    /// Configuration.
    config: FetchingConfig,
    /// Per-slot envelope state.
    slots: DashMap<SlotIndex, SlotEnvelopes>,
    /// TxSet fetcher.
    tx_set_fetcher: ItemFetcher,
    /// QuorumSet fetcher.
    quorum_set_fetcher: ItemFetcher,
    /// Cached TxSets (hash -> (slot, data)).
    tx_set_cache: DashMap<Hash256, (SlotIndex, Vec<u8>)>,
    /// Cached QuorumSets (hash -> data).
    quorum_set_cache: DashMap<Hash256, Arc<ScpQuorumSet>>,
    /// Callback to broadcast envelopes when they become ready.
    ///
    /// Parity: stellar-core broadcasts envelopes to peers via
    /// `OverlayManager::broadcastMessage()` when dependencies are satisfied.
    broadcast: RwLock<Option<BroadcastFn>>,
    /// Statistics.
    stats: RwLock<FetchingStats>,
}

/// Statistics about fetching.
#[derive(Debug, Clone, Default)]
pub struct FetchingStats {
    /// Total envelopes received.
    pub envelopes_received: u64,
    /// Envelopes that were immediately ready.
    pub envelopes_ready: u64,
    /// Envelopes waiting for fetching.
    pub envelopes_fetching: u64,
    /// Envelopes that were duplicates.
    pub envelopes_duplicate: u64,
    /// TxSets received.
    pub tx_sets_received: u64,
    /// QuorumSets received.
    pub quorum_sets_received: u64,
}

impl FetchingEnvelopes {
    /// Create a new fetching envelopes manager.
    pub fn new(config: FetchingConfig) -> Self {
        Self {
            tx_set_fetcher: ItemFetcher::new(ItemType::TxSet, config.tx_set_fetcher_config.clone()),
            quorum_set_fetcher: ItemFetcher::new(
                ItemType::QuorumSet,
                config.quorum_set_fetcher_config.clone(),
            ),
            config,
            slots: DashMap::new(),
            tx_set_cache: DashMap::new(),
            quorum_set_cache: DashMap::new(),
            broadcast: RwLock::new(None),
            stats: RwLock::new(FetchingStats::default()),
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(FetchingConfig::default())
    }

    /// Set the callback for requesting TxSets from peers.
    pub fn set_tx_set_ask_peer(&mut self, f: AskPeerFn) {
        self.tx_set_fetcher.set_ask_peer(f);
    }

    /// Set the callback for requesting QuorumSets from peers.
    pub fn set_quorum_set_ask_peer(&mut self, f: AskPeerFn) {
        self.quorum_set_fetcher.set_ask_peer(f);
    }

    /// Set the callback for broadcasting ready envelopes to peers.
    ///
    /// Parity: stellar-core broadcasts envelopes when their dependencies
    /// are satisfied in `PendingEnvelopes::startFetch()`.
    pub fn set_broadcast<F>(&self, f: F)
    where
        F: Fn(&ScpEnvelope) + Send + Sync + 'static,
    {
        *self.broadcast.write() = Some(Box::new(f));
    }

    /// Update the list of available peers.
    pub fn set_available_peers(&self, peers: Vec<PeerId>) {
        self.tx_set_fetcher.set_available_peers(peers.clone());
        self.quorum_set_fetcher.set_available_peers(peers);
    }

    /// Receive an SCP envelope.
    ///
    /// Returns the result indicating whether the envelope is ready, fetching,
    /// or was already processed/discarded.
    pub fn recv_envelope(&self, envelope: ScpEnvelope) -> RecvResult {
        let slot = envelope.statement.slot_index;
        let env_hash = Self::compute_envelope_hash(&envelope);

        self.stats.write().envelopes_received += 1;

        // Parity: reject envelopes with non-SIGNED StellarValues
        if !Self::check_stellar_value_signed(&envelope) {
            debug!(slot, "Rejecting envelope with non-SIGNED StellarValue");
            return RecvResult::Discarded;
        }

        // Get or create slot state
        let mut slot_state = self.slots.entry(slot).or_default();

        // Check if already processed or discarded
        if slot_state.processed.contains(&env_hash) || slot_state.discarded.contains(&env_hash) {
            self.stats.write().envelopes_duplicate += 1;
            return RecvResult::AlreadyProcessed;
        }

        // Check if already fetching
        if slot_state.fetching.contains_key(&env_hash) {
            return RecvResult::Fetching;
        }

        // Check if we have all dependencies
        let (need_tx_set, need_quorum_set) = self.check_dependencies(&envelope);

        if !need_tx_set && !need_quorum_set {
            // All dependencies available - envelope is ready
            // Parity: broadcast to peers when dependencies are satisfied
            self.broadcast_envelope(&envelope);
            slot_state.ready.push(envelope);
            self.stats.write().envelopes_ready += 1;
            return RecvResult::Ready;
        }

        // Start fetching missing dependencies
        if need_tx_set {
            for tx_set_hash in Self::extract_tx_set_hashes(&envelope) {
                if !self.tx_set_cache.contains_key(&tx_set_hash) {
                    let hash = Hash(tx_set_hash.0);
                    self.tx_set_fetcher.fetch(hash, &envelope);
                }
            }
        }

        if need_quorum_set {
            if let Some(qs_hash) = Self::extract_quorum_set_hash(&envelope) {
                let hash = Hash(qs_hash.0);
                self.quorum_set_fetcher.fetch(hash, &envelope);
            }
        }

        // Add to fetching
        slot_state
            .fetching
            .insert(env_hash, (envelope, Instant::now()));
        self.stats.write().envelopes_fetching += 1;

        RecvResult::Fetching
    }

    /// Receive a TxSet.
    ///
    /// Returns true if the TxSet was needed and envelopes may be ready.
    pub fn recv_tx_set(&self, hash: Hash256, slot: SlotIndex, data: Vec<u8>) -> bool {
        // Check if we're fetching this TxSet
        if !self.tx_set_fetcher.is_tracking(&Hash(hash.0)) {
            trace!("Received unrequested TxSet {}", hex::encode(hash.0));
            return false;
        }

        self.stats.write().tx_sets_received += 1;

        // Evict old entries if cache is full
        self.evict_tx_set_cache_if_full();

        // Cache the TxSet
        self.tx_set_cache.insert(hash, (slot, data));

        // Notify the fetcher and get waiting envelopes
        let waiting = self.tx_set_fetcher.recv(&Hash(hash.0));

        debug!(
            "Received TxSet {}, {} envelopes waiting",
            hex::encode(hash.0),
            waiting.len()
        );

        // Re-process waiting envelopes
        for env in waiting {
            self.check_and_move_to_ready(env);
        }

        true
    }

    /// Receive a QuorumSet.
    ///
    /// Returns true if the QuorumSet was needed and envelopes may be ready.
    pub fn recv_quorum_set(&self, hash: Hash256, quorum_set: ScpQuorumSet) -> bool {
        // Check if we're fetching this QuorumSet
        if !self.quorum_set_fetcher.is_tracking(&Hash(hash.0)) {
            trace!("Received unrequested QuorumSet {}", hex::encode(hash.0));
            return false;
        }

        // Parity: reject insane quorum sets before caching
        if let Err(reason) = is_quorum_set_sane(&quorum_set, false) {
            debug!(
                hash = %hex::encode(hash.0),
                reason = %reason,
                "Rejecting insane QuorumSet"
            );
            // Stop tracking this hash so fetching envelopes that depend on it
            // eventually time out rather than wait forever
            self.quorum_set_fetcher.recv(&Hash(hash.0));
            return false;
        }

        self.stats.write().quorum_sets_received += 1;

        // Evict old entries if cache is full
        self.evict_quorum_set_cache_if_full();

        // Cache the QuorumSet
        self.quorum_set_cache.insert(hash, Arc::new(quorum_set));

        // Notify the fetcher and get waiting envelopes
        let waiting = self.quorum_set_fetcher.recv(&Hash(hash.0));

        debug!(
            "Received QuorumSet {}, {} envelopes waiting",
            hex::encode(hash.0),
            waiting.len()
        );

        // Re-process waiting envelopes
        for env in waiting {
            self.check_and_move_to_ready(env);
        }

        true
    }

    /// Handle DONT_HAVE response from a peer.
    pub fn peer_doesnt_have(&self, item_type: ItemType, hash: Hash256, peer: &PeerId) {
        match item_type {
            ItemType::TxSet => {
                self.tx_set_fetcher.doesnt_have(&Hash(hash.0), peer);
            }
            ItemType::QuorumSet => {
                self.quorum_set_fetcher.doesnt_have(&Hash(hash.0), peer);
            }
        }
    }

    /// Pop a ready envelope from the lowest slot up to `max_slot`.
    ///
    /// Parity: stellar-core iterates from lowest slot to `slotIndex` and
    /// returns the first available ready envelope. This ensures envelopes
    /// are processed in slot order.
    pub fn pop(&self, max_slot: SlotIndex) -> Option<ScpEnvelope> {
        // Collect slots that have ready envelopes, up to max_slot
        let mut ready: Vec<SlotIndex> = self
            .slots
            .iter()
            .filter(|e| *e.key() <= max_slot && !e.value().ready.is_empty())
            .map(|e| *e.key())
            .collect();
        ready.sort_unstable();

        for slot in ready {
            if let Some(mut slot_state) = self.slots.get_mut(&slot) {
                if let Some(envelope) = slot_state.ready.pop() {
                    let env_hash = Self::compute_envelope_hash(&envelope);
                    slot_state.processed.insert(env_hash);
                    return Some(envelope);
                }
            }
        }

        None
    }

    /// Test-only: directly insert envelopes into a slot's ready queue.
    ///
    /// Bypasses `recv_envelope`'s signature/dependency checks so tests can
    /// synthesise a large pre-ready backlog (e.g. to measure drain latency
    /// for #1773 regression coverage) without constructing valid signed
    /// StellarValues and a matching quorum-set graph.
    ///
    /// Not for production use: callers of `recv_envelope` get the full
    /// dependency resolution pipeline; this helper skips it entirely.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn test_insert_ready(&self, slot: SlotIndex, envelopes: Vec<ScpEnvelope>) {
        let mut slot_state = self.slots.entry(slot).or_default();
        slot_state.ready.extend(envelopes);
    }

    /// Get all ready slots in ascending order.
    ///
    /// Parity: stellar-core returns slots in sorted order since it uses std::map.
    pub fn ready_slots(&self) -> Vec<SlotIndex> {
        let mut slots: Vec<SlotIndex> = self
            .slots
            .iter()
            .filter(|entry| !entry.value().ready.is_empty())
            .map(|entry| *entry.key())
            .collect();
        slots.sort_unstable();
        slots
    }

    /// Erase data for slots below the given threshold.
    pub fn erase_below(&self, slot_index: SlotIndex, slot_to_keep: SlotIndex) {
        // Remove old slots
        let slots_to_remove: Vec<SlotIndex> = self
            .slots
            .iter()
            .filter(|e| *e.key() < slot_index && *e.key() != slot_to_keep)
            .map(|e| *e.key())
            .collect();

        for slot in slots_to_remove {
            self.slots.remove(&slot);
        }

        // Evict old tx sets from cache (keeps memory bounded)
        let tx_sets_to_remove: Vec<Hash256> = self
            .tx_set_cache
            .iter()
            .filter(|e| e.value().0 < slot_index && e.value().0 != slot_to_keep)
            .map(|e| *e.key())
            .collect();

        let evicted_count = tx_sets_to_remove.len();
        for hash in tx_sets_to_remove {
            self.tx_set_cache.remove(&hash);
        }

        if evicted_count > 0 {
            debug!(
                evicted = evicted_count,
                slot_index,
                remaining = self.tx_set_cache.len(),
                "Evicted old TxSets from cache in erase_below"
            );
        }

        // Tell fetchers to stop fetching for old slots
        self.tx_set_fetcher
            .stop_fetching_below(slot_index, slot_to_keep);
        self.quorum_set_fetcher
            .stop_fetching_below(slot_index, slot_to_keep);
    }

    /// Process pending fetch requests (should be called periodically).
    pub fn process_pending(&self) -> usize {
        let tx_sent = self.tx_set_fetcher.process_pending();
        let qs_sent = self.quorum_set_fetcher.process_pending();
        tx_sent + qs_sent
    }

    /// Get statistics.
    pub fn stats(&self) -> FetchingStats {
        self.stats.read().clone()
    }

    /// Trim stale data while preserving tx_sets for slots after catchup.
    /// Called after catchup to release memory from stale data while keeping
    /// tx_sets that will be needed for the ledgers immediately after catchup.
    ///
    /// This is critical for avoiding sync gaps: during catchup, we receive
    /// EXTERNALIZE envelopes and cache their tx_sets. After catchup completes,
    /// we need those tx_sets to apply the buffered ledgers. If we clear them,
    /// peers may have already evicted those old tx_sets, causing "DontHave"
    /// responses and sync failures.
    pub fn trim_stale(&self, keep_after_slot: SlotIndex) {
        let initial_tx_set_count = self.tx_set_cache.len();
        let initial_quorum_set_count = self.quorum_set_cache.len();
        let initial_slots_count = self.slots.len();

        // Clear slots for old ledgers only
        self.slots.retain(|slot, _| *slot > keep_after_slot);

        // Clear tx_sets for old ledgers only - KEEP tx_sets for slots > keep_after_slot
        self.tx_set_cache
            .retain(|_, (slot, _)| *slot > keep_after_slot);

        // Keep quorum_set_cache — quorum sets are small, don't have slot-based
        // relevance, and are needed by check_dependencies() to determine if
        // envelopes arriving after trim are ready. Clearing them causes
        // EXTERNALIZE envelopes to be stuck in fetching state even after
        // their tx_set arrives, because the quorum set dependency appears unmet.

        // Clear tx_set fetcher — pending requests for old slots are stale.
        self.tx_set_fetcher.clear();
        // Keep quorum_set_fetcher — same reasoning as quorum_set_cache above.
        // Clearing the fetcher discards trackers for in-flight quorum set
        // requests, so envelopes waiting on those quorum sets would be stuck
        // in "fetching" state until a new envelope re-triggers the fetch.

        let kept_tx_sets = self.tx_set_cache.len();
        let kept_slots = self.slots.len();

        tracing::info!(
            initial_tx_set_count,
            initial_quorum_set_count,
            initial_slots_count,
            kept_tx_sets,
            kept_slots,
            keep_after_slot,
            "Trimmed stale fetching_envelopes caches, preserving future tx_sets"
        );
    }

    /// Get the number of cached TxSets.
    pub fn tx_set_cache_size(&self) -> usize {
        self.tx_set_cache.len()
    }

    /// Get the number of cached QuorumSets.
    pub fn quorum_set_cache_size(&self) -> usize {
        self.quorum_set_cache.len()
    }

    /// Get the number of slots being tracked.
    pub fn slots_count(&self) -> usize {
        self.slots.len()
    }

    /// Get the number of envelopes being fetched.
    pub fn fetching_count(&self) -> usize {
        self.slots
            .iter()
            .map(|entry| entry.value().fetching.len())
            .sum()
    }

    /// Get the number of ready envelopes.
    pub fn ready_count(&self) -> usize {
        self.slots
            .iter()
            .map(|entry| entry.value().ready.len())
            .sum()
    }

    /// Check if a slot has envelopes currently being fetched (waiting for tx_set).
    pub fn has_fetching_for_slot(&self, slot: SlotIndex) -> bool {
        self.slots
            .get(&slot)
            .map(|entry| !entry.value().fetching.is_empty())
            .unwrap_or(false)
    }

    /// Check if we have a cached TxSet.
    pub fn has_tx_set(&self, hash: &Hash256) -> bool {
        self.tx_set_cache.contains_key(hash)
    }

    /// Get a cached TxSet.
    pub fn get_tx_set(&self, hash: &Hash256) -> Option<Vec<u8>> {
        self.tx_set_cache.get(hash).map(|e| e.value().1.clone())
    }

    /// Check if we have a cached QuorumSet.
    pub fn has_quorum_set(&self, hash: &Hash256) -> bool {
        self.quorum_set_cache.contains_key(hash)
    }

    /// Get a cached QuorumSet.
    pub fn get_quorum_set(&self, hash: &Hash256) -> Option<Arc<ScpQuorumSet>> {
        self.quorum_set_cache.get(hash).map(|e| e.value().clone())
    }

    /// Add a TxSet to the cache directly (e.g., locally created).
    pub fn cache_tx_set(&self, hash: Hash256, slot: SlotIndex, data: Vec<u8>) {
        self.evict_tx_set_cache_if_full();
        self.tx_set_cache.insert(hash, (slot, data));
    }

    /// Mark a TxSet as available and process any waiting envelopes.
    ///
    /// This is used when we receive a tx set through other means (not the fetcher)
    /// but want to notify waiting envelopes that the dependency is satisfied.
    pub fn tx_set_available(&self, hash: Hash256, slot: SlotIndex) {
        // Cache it (eviction handled in cache_tx_set)
        self.evict_tx_set_cache_if_full();
        self.tx_set_cache.insert(hash, (slot, Vec::new()));

        // Try recv_tx_set which handles tracked items
        // (if not tracked, it will return early which is fine)
        let _ = self.recv_tx_set(hash, slot, Vec::new());

        // Also manually check all fetching envelopes for this hash
        // in case they weren't tracked through the fetcher
        self.move_ready_envelopes_for_tx_set(&hash);
    }

    /// Check all fetching envelopes and move any that are now ready.
    fn move_ready_envelopes_for_tx_set(&self, tx_set_hash: &Hash256) {
        // Phase 1: Collect envelopes under read guards only.
        // Scoped to ensure all RefMulti (per-shard read guards) drop
        // before phase 2 acquires write locks via get_mut().
        let to_check: Vec<ScpEnvelope> = {
            let mut result = Vec::new();
            for slot_entry in self.slots.iter() {
                for (_, (env, _)) in slot_entry.fetching.iter() {
                    if Self::extract_tx_set_hashes(env)
                        .iter()
                        .any(|h| h == tx_set_hash)
                    {
                        result.push(env.clone());
                    }
                }
            }
            result
        }; // ← All RefMulti guards dropped here

        // Phase 2: Mutate with no read guards held.
        for envelope in to_check {
            self.check_and_move_to_ready(envelope);
        }
    }

    /// Check all fetching envelopes referencing a quorum-set hash and move
    /// any that are now ready.
    fn move_ready_envelopes_for_quorum_set(&self, qs_hash: &Hash256) {
        let to_check: Vec<ScpEnvelope> = {
            let mut result = Vec::new();
            for slot_entry in self.slots.iter() {
                for (_, (env, _)) in slot_entry.fetching.iter() {
                    if Self::extract_quorum_set_hash(env).as_ref() == Some(qs_hash) {
                        result.push(env.clone());
                    }
                }
            }
            result
        };

        for envelope in to_check {
            self.check_and_move_to_ready(envelope);
        }
    }

    /// Add a QuorumSet to the cache directly and process any waiting envelopes.
    ///
    /// Parallel to `tx_set_available` — used when a quorum-set is received
    /// through means other than the quorum-set fetcher.
    pub fn cache_quorum_set(&self, hash: Hash256, quorum_set: ScpQuorumSet) {
        self.evict_quorum_set_cache_if_full();
        self.quorum_set_cache.insert(hash, Arc::new(quorum_set));

        // Check if any fetching envelopes are now ready
        self.move_ready_envelopes_for_quorum_set(&hash);
    }

    // --- Internal helpers ---

    /// Evict a tx set cache entry if the cache is full.
    ///
    /// Prefers evicting entries NOT referenced by any fetching envelope to avoid
    /// stranding envelopes that already received the tx-set but are still waiting
    /// on another dependency (e.g. quorum-set). Falls back to oldest-slot eviction
    /// when all entries are referenced to preserve the hard memory bound.
    fn evict_tx_set_cache_if_full(&self) {
        if self.tx_set_cache.len() < self.config.max_tx_set_cache {
            return;
        }

        // Collect tx-set hashes referenced by currently-fetching envelopes
        let referenced: HashSet<Hash256> = self
            .slots
            .iter()
            .flat_map(|slot_entry| {
                slot_entry
                    .fetching
                    .values()
                    .flat_map(|(env, _)| Self::extract_tx_set_hashes(env))
                    .collect::<Vec<_>>()
            })
            .collect();

        // Prefer evicting an unreferenced entry
        let to_remove = self
            .tx_set_cache
            .iter()
            .find(|entry| !referenced.contains(entry.key()))
            .map(|entry| *entry.key())
            .or_else(|| {
                // Fallback: evict the oldest-slot entry to preserve memory bound
                self.tx_set_cache
                    .iter()
                    .min_by_key(|entry| entry.value().0)
                    .map(|entry| *entry.key())
            });

        if let Some(hash) = to_remove {
            self.tx_set_cache.remove(&hash);
            debug!(
                cache_size = self.tx_set_cache.len(),
                "Evicted TxSet from cache"
            );
        }
    }

    /// Evict a quorum set cache entry if the cache is full.
    ///
    /// Prefers evicting entries NOT referenced by any fetching envelope.
    /// Falls back to arbitrary eviction to preserve the hard memory bound.
    fn evict_quorum_set_cache_if_full(&self) {
        if self.quorum_set_cache.len() < self.config.max_quorum_set_cache {
            return;
        }

        // Collect quorum-set hashes referenced by currently-fetching envelopes
        let referenced: HashSet<Hash256> = self
            .slots
            .iter()
            .flat_map(|slot_entry| {
                slot_entry
                    .fetching
                    .values()
                    .filter_map(|(env, _)| Self::extract_quorum_set_hash(env))
                    .collect::<Vec<_>>()
            })
            .collect();

        // Prefer evicting an unreferenced entry
        let to_remove = self
            .quorum_set_cache
            .iter()
            .find(|entry| !referenced.contains(entry.key()))
            .map(|entry| *entry.key())
            .or_else(|| {
                // Fallback: evict an arbitrary entry to preserve memory bound
                self.quorum_set_cache.iter().next().map(|e| *e.key())
            });

        if let Some(hash) = to_remove {
            self.quorum_set_cache.remove(&hash);
            debug!(
                cache_size = self.quorum_set_cache.len(),
                "Evicted QuorumSet from cache"
            );
        }
    }

    /// Check what dependencies are missing for an envelope.
    ///
    /// Parity: checks both tx set and quorum set dependencies.
    /// An envelope is ready only when all referenced data is cached.
    fn check_dependencies(&self, envelope: &ScpEnvelope) -> (bool, bool) {
        let tx_set_hashes = Self::extract_tx_set_hashes(envelope);
        let need_tx_set = tx_set_hashes
            .iter()
            .any(|hash| !self.tx_set_cache.contains_key(hash));

        let need_quorum_set = if let Some(hash) = Self::extract_quorum_set_hash(envelope) {
            !self.quorum_set_cache.contains_key(&hash)
        } else {
            false
        };

        (need_tx_set, need_quorum_set)
    }

    /// Check if an envelope is now fully fetched and move to ready if so.
    ///
    /// If dependencies are still missing (e.g. a previously-received dependency
    /// was evicted from the cache under pressure), re-start fetching for those
    /// dependencies. This mirrors stellar-core's `startFetch()` retry behavior
    /// in `PendingEnvelopes::recvSCPEnvelope`.
    fn check_and_move_to_ready(&self, envelope: ScpEnvelope) {
        let slot = envelope.statement.slot_index;
        let env_hash = Self::compute_envelope_hash(&envelope);

        let (need_tx_set, need_quorum_set) = self.check_dependencies(&envelope);

        if !need_tx_set && !need_quorum_set {
            // All dependencies available - move to ready
            // Parity: broadcast to peers when dependencies are satisfied
            self.broadcast_envelope(&envelope);
            if let Some(mut slot_state) = self.slots.get_mut(&slot) {
                if slot_state.fetching.remove(&env_hash).is_some() {
                    slot_state.ready.push(envelope);
                    debug!(slot, "Envelope ready after receiving dependencies");
                }
            }
        } else {
            // Re-start fetching for any dependency that was evicted from the
            // cache after being received. Without this, the envelope would
            // remain stranded in `fetching` forever.
            if need_tx_set {
                for tx_set_hash in Self::extract_tx_set_hashes(&envelope) {
                    if !self.tx_set_cache.contains_key(&tx_set_hash) {
                        let hash = Hash(tx_set_hash.0);
                        self.tx_set_fetcher.fetch(hash, &envelope);
                        debug!(
                            slot,
                            tx_set = %hex::encode(tx_set_hash.0),
                            "Re-started fetch for evicted tx-set dependency"
                        );
                    }
                }
            }
            if need_quorum_set {
                if let Some(qs_hash) = Self::extract_quorum_set_hash(&envelope) {
                    if !self.quorum_set_cache.contains_key(&qs_hash) {
                        let hash = Hash(qs_hash.0);
                        self.quorum_set_fetcher.fetch(hash, &envelope);
                        debug!(
                            slot,
                            quorum_set = %hex::encode(qs_hash.0),
                            "Re-started fetch for evicted quorum-set dependency"
                        );
                    }
                }
            }
        }
    }

    /// Broadcast an envelope to peers if a broadcast callback is set.
    fn broadcast_envelope(&self, envelope: &ScpEnvelope) {
        if let Some(ref broadcast) = *self.broadcast.read() {
            broadcast(envelope);
        }
    }

    /// Check that all StellarValues in an envelope have STELLAR_VALUE_SIGNED ext.
    ///
    /// Parity: stellar-core rejects envelopes containing non-signed StellarValues
    /// in both nomination (votes/accepted) and ballot statements.
    fn check_stellar_value_signed(envelope: &ScpEnvelope) -> bool {
        use stellar_xdr::curr::{ScpStatementPledges, StellarValue, StellarValueExt};

        let values: Vec<&[u8]> = match &envelope.statement.pledges {
            ScpStatementPledges::Nominate(nom) => {
                // Check all voted and accepted values
                nom.votes
                    .iter()
                    .chain(nom.accepted.iter())
                    .map(|v| v.0.as_slice())
                    .collect()
            }
            ScpStatementPledges::Prepare(prep) => {
                let mut vals = vec![prep.ballot.value.0.as_slice()];
                if let Some(ref prepared) = prep.prepared {
                    vals.push(prepared.value.0.as_slice());
                }
                if let Some(ref prepared_prime) = prep.prepared_prime {
                    vals.push(prepared_prime.value.0.as_slice());
                }
                vals
            }
            ScpStatementPledges::Confirm(conf) => vec![conf.ballot.value.0.as_slice()],
            ScpStatementPledges::Externalize(ext) => vec![ext.commit.value.0.as_slice()],
        };

        for value_bytes in values {
            match StellarValue::from_xdr(value_bytes, Limits::none()) {
                Ok(sv) => {
                    if matches!(sv.ext, StellarValueExt::Basic) {
                        return false;
                    }
                }
                Err(_) => {
                    // Can't decode — reject
                    return false;
                }
            }
        }

        true
    }

    /// Extract all TxSet hashes from an envelope.
    ///
    /// Parity: stellar-core's `getValidatedTxSetHashes` extracts tx_set hashes
    /// from ALL statement types including NOMINATE (votes + accepted). Previously
    /// NOMINATE returned None, bypassing tx_set fetch gating (#1117).
    fn extract_tx_set_hashes(envelope: &ScpEnvelope) -> Vec<Hash256> {
        use stellar_xdr::curr::StellarValue;

        henyey_scp::Slot::get_statement_values(&envelope.statement)
            .into_iter()
            .filter_map(|v| StellarValue::from_xdr(&v.0, Limits::none()).ok())
            .map(|sv| Hash256::from_bytes(sv.tx_set_hash.0))
            .collect()
    }

    /// Extract QuorumSet hash from an envelope.
    fn extract_quorum_set_hash(envelope: &ScpEnvelope) -> Option<Hash256> {
        use stellar_xdr::curr::ScpStatementPledges;

        let hash = match &envelope.statement.pledges {
            ScpStatementPledges::Nominate(nom) => Some(&nom.quorum_set_hash),
            ScpStatementPledges::Prepare(prep) => Some(&prep.quorum_set_hash),
            ScpStatementPledges::Confirm(conf) => Some(&conf.quorum_set_hash),
            ScpStatementPledges::Externalize(ext) => Some(&ext.commit_quorum_set_hash),
        };

        hash.map(|h| Hash256::from_bytes(h.0))
    }

    /// Compute hash of an envelope for deduplication.
    fn compute_envelope_hash(envelope: &ScpEnvelope) -> Hash256 {
        Hash256::hash_xdr(envelope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        NodeId as XdrNodeId, PublicKey, ScpNomination, ScpStatement, ScpStatementPledges,
        Signature, Uint256, Value, WriteXdr,
    };

    fn make_test_envelope(slot: SlotIndex, node_seed: u8) -> ScpEnvelope {
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([node_seed; 32])));

        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash([1u8; 32]),
                    votes: vec![].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: Signature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    /// Helper to pre-cache the quorum set used by test envelopes.
    ///
    /// NOTE: This directly populates the cache and does NOT go through
    /// `recv_quorum_set()`. It will NOT trigger the fetcher→recheck→broadcast
    /// path. Use `recv_quorum_set()` when testing that path.
    fn cache_test_quorum_set(fetching: &FetchingEnvelopes) {
        let qs_hash = Hash256::from_bytes([1u8; 32]);
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        fetching.cache_quorum_set(
            qs_hash,
            ScpQuorumSet {
                threshold: 1,
                validators: vec![node_id].try_into().unwrap(),
                inner_sets: vec![].try_into().unwrap(),
            },
        );
    }

    #[test]
    fn test_recv_envelope_nomination_needs_quorum_set() {
        let fetching = FetchingEnvelopes::with_defaults();

        // Without pre-caching the quorum set, nomination envelopes need fetching
        let envelope = make_test_envelope(100, 1);
        let result = fetching.recv_envelope(envelope);
        assert_eq!(result, RecvResult::Fetching);
    }

    #[test]
    fn test_recv_envelope_ready_when_cached() {
        let fetching = FetchingEnvelopes::with_defaults();

        // Pre-cache the quorum set so envelope is immediately ready
        cache_test_quorum_set(&fetching);

        let envelope = make_test_envelope(100, 1);
        let result = fetching.recv_envelope(envelope);

        assert_eq!(result, RecvResult::Ready);
        assert_eq!(fetching.ready_count(), 1);
    }

    #[test]
    fn test_cache_quorum_set() {
        let fetching = FetchingEnvelopes::with_defaults();

        // Verify we can cache and retrieve quorum sets
        let qs_hash = Hash256::from_bytes([1u8; 32]);
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        fetching.cache_quorum_set(qs_hash, quorum_set);

        assert!(fetching.has_quorum_set(&qs_hash));
        let retrieved = fetching.get_quorum_set(&qs_hash);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().threshold, 1);
    }

    #[test]
    fn test_pop_marks_as_processed() {
        let fetching = FetchingEnvelopes::with_defaults();
        cache_test_quorum_set(&fetching);

        let envelope = make_test_envelope(100, 1);
        fetching.recv_envelope(envelope.clone());

        // Pop the envelope
        let popped = fetching.pop(100);
        assert!(popped.is_some());

        // Adding same envelope again should be duplicate
        let result = fetching.recv_envelope(envelope);
        assert_eq!(result, RecvResult::AlreadyProcessed);
    }

    #[test]
    fn test_erase_below() {
        let fetching = FetchingEnvelopes::with_defaults();
        cache_test_quorum_set(&fetching);

        // Add envelopes for different slots
        fetching.recv_envelope(make_test_envelope(100, 1));
        fetching.recv_envelope(make_test_envelope(101, 2));
        fetching.recv_envelope(make_test_envelope(102, 3));

        assert_eq!(fetching.ready_count(), 3);

        // Erase below 102, keeping 100
        fetching.erase_below(102, 100);

        // Slot 100 should be kept, 101 erased, 102 kept
        let ready_slots = fetching.ready_slots();
        assert!(ready_slots.contains(&100));
        assert!(!ready_slots.contains(&101));
        assert!(ready_slots.contains(&102));
    }

    #[test]
    fn test_trim_stale_preserves_future_tx_sets() {
        let fetching = FetchingEnvelopes::with_defaults();

        // Create distinct hashes for each tx_set
        let hash_old = Hash256::from_bytes([98u8; 32]);
        let hash_boundary = Hash256::from_bytes([99u8; 32]);
        let hash_future = Hash256::from_bytes([100u8; 32]);

        // Cache tx_sets for various slots (hash, slot, data)
        fetching.cache_tx_set(hash_old, 98, vec![1, 2, 3]);
        fetching.cache_tx_set(hash_boundary, 100, vec![4, 5, 6]);
        fetching.cache_tx_set(hash_future, 101, vec![7, 8, 9]);

        // Also add some quorum sets
        let qs_hash = Hash256::from_bytes([1u8; 32]);
        fetching.cache_quorum_set(
            qs_hash,
            ScpQuorumSet {
                threshold: 1,
                validators: vec![].try_into().unwrap(),
                inner_sets: vec![].try_into().unwrap(),
            },
        );

        // Verify all are cached before trim
        assert!(fetching.get_tx_set(&hash_old).is_some());
        assert!(fetching.get_tx_set(&hash_boundary).is_some());
        assert!(fetching.get_tx_set(&hash_future).is_some());
        assert!(fetching.has_quorum_set(&qs_hash));

        // Trim with keep_after_slot = 100
        // Should keep tx_sets for slots > 100, i.e., slot 101
        fetching.trim_stale(100);

        // Verify tx_sets: only future slot (101) should remain
        assert!(
            fetching.get_tx_set(&hash_future).is_some(),
            "tx_set for slot 101 should be preserved"
        );
        assert!(
            fetching.get_tx_set(&hash_old).is_none(),
            "tx_set for slot 98 should be removed"
        );
        assert!(
            fetching.get_tx_set(&hash_boundary).is_none(),
            "tx_set for slot 100 should be removed (boundary)"
        );

        // Verify quorum_set_cache is preserved (needed by check_dependencies
        // for envelopes arriving after trim)
        assert!(
            fetching.has_quorum_set(&qs_hash),
            "quorum sets should be preserved after trim"
        );
    }

    // =========================================================================
    // Phase 1A: Quorum Set Sanity Validation
    // =========================================================================

    #[test]
    fn test_recv_quorum_set_rejects_insane() {
        let fetching = FetchingEnvelopes::with_defaults();

        // First, make the fetcher track a hash so recv_quorum_set doesn't
        // short-circuit with "unrequested".
        let qs_hash = Hash256::from_bytes([42u8; 32]);

        // Submit an envelope that needs this quorum set hash so the fetcher tracks it
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let envelope = ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: 100,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash(qs_hash.0),
                    votes: vec![].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: Signature(vec![0u8; 64].try_into().unwrap()),
        };
        let result = fetching.recv_envelope(envelope);
        assert_eq!(result, RecvResult::Fetching);

        // Now receive an insane quorum set (threshold > validators count)
        let insane_qs = ScpQuorumSet {
            threshold: 5, // threshold exceeds number of validators
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let accepted = fetching.recv_quorum_set(qs_hash, insane_qs);
        assert!(!accepted, "Insane quorum set should be rejected");

        // The quorum set should NOT be cached
        assert!(!fetching.has_quorum_set(&qs_hash));
    }

    #[test]
    fn test_recv_quorum_set_accepts_sane() {
        let fetching = FetchingEnvelopes::with_defaults();
        let qs_hash = Hash256::from_bytes([42u8; 32]);

        // Submit an envelope to make fetcher track the hash
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let envelope = ScpEnvelope {
            statement: ScpStatement {
                node_id: node_id.clone(),
                slot_index: 100,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash(qs_hash.0),
                    votes: vec![].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: Signature(vec![0u8; 64].try_into().unwrap()),
        };
        fetching.recv_envelope(envelope);

        // Receive a sane quorum set
        let sane_qs = ScpQuorumSet {
            threshold: 1,
            validators: vec![node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let accepted = fetching.recv_quorum_set(qs_hash, sane_qs);
        assert!(accepted, "Sane quorum set should be accepted");
        assert!(fetching.has_quorum_set(&qs_hash));
    }

    // =========================================================================
    // Phase 1C: STELLAR_VALUE_SIGNED check
    // =========================================================================

    fn make_basic_stellar_value() -> Vec<u8> {
        use stellar_xdr::curr::{StellarValue, StellarValueExt, TimePoint, VecM, WriteXdr};
        let sv = StellarValue {
            tx_set_hash: Hash([0u8; 32]),
            close_time: TimePoint(12345),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        };
        sv.to_xdr(Limits::none()).unwrap()
    }

    fn make_signed_stellar_value_bytes() -> Vec<u8> {
        use stellar_xdr::curr::{
            LedgerCloseValueSignature, StellarValue, StellarValueExt, TimePoint, VecM, WriteXdr,
        };
        let sv = StellarValue {
            tx_set_hash: Hash([0u8; 32]),
            close_time: TimePoint(12345),
            upgrades: VecM::default(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id: XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
            }),
        };
        sv.to_xdr(Limits::none()).unwrap()
    }

    fn make_envelope_with_nomination_values(slot: SlotIndex, values: Vec<Vec<u8>>) -> ScpEnvelope {
        use stellar_xdr::curr::Value;
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let votes: Vec<Value> = values
            .into_iter()
            .map(|v| Value(v.try_into().unwrap()))
            .collect();
        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash([1u8; 32]),
                    votes: votes.try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: Signature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    #[test]
    fn test_recv_envelope_rejects_basic_stellar_value() {
        let fetching = FetchingEnvelopes::with_defaults();
        cache_test_quorum_set(&fetching);

        // Envelope with a nomination containing a Basic StellarValue
        let envelope = make_envelope_with_nomination_values(100, vec![make_basic_stellar_value()]);
        let result = fetching.recv_envelope(envelope);
        assert_eq!(
            result,
            RecvResult::Discarded,
            "Envelopes with Basic StellarValue should be discarded"
        );
    }

    #[test]
    fn test_recv_envelope_accepts_signed_stellar_value() {
        let fetching = FetchingEnvelopes::with_defaults();
        cache_test_quorum_set(&fetching);

        // Pre-cache the tx_set referenced by the signed value (hash [0u8; 32])
        fetching.cache_tx_set(Hash256::from_bytes([0u8; 32]), 100, vec![]);

        // Envelope with a nomination containing a Signed StellarValue
        let envelope =
            make_envelope_with_nomination_values(100, vec![make_signed_stellar_value_bytes()]);
        let result = fetching.recv_envelope(envelope);
        assert_eq!(
            result,
            RecvResult::Ready,
            "Envelopes with Signed StellarValue should be accepted"
        );
    }

    #[test]
    fn test_recv_envelope_rejects_undecoded_value() {
        let fetching = FetchingEnvelopes::with_defaults();
        cache_test_quorum_set(&fetching);

        // Envelope with garbage bytes that can't be decoded as StellarValue
        let envelope = make_envelope_with_nomination_values(100, vec![vec![0xFF, 0xFE, 0xFD]]);
        let result = fetching.recv_envelope(envelope);
        assert_eq!(
            result,
            RecvResult::Discarded,
            "Envelopes with undecodable values should be discarded"
        );
    }

    // =========================================================================
    // Phase 1E: pop() ordering and ready_slots() sorting
    // =========================================================================

    #[test]
    fn test_pop_returns_lowest_slot_first() {
        let fetching = FetchingEnvelopes::with_defaults();
        cache_test_quorum_set(&fetching);

        // Add envelopes in non-sequential order: slot 300, 100, 200
        fetching.recv_envelope(make_test_envelope(300, 1));
        fetching.recv_envelope(make_test_envelope(100, 2));
        fetching.recv_envelope(make_test_envelope(200, 3));

        // Pop should return slot 100 first (lowest)
        let first = fetching.pop(u64::MAX).unwrap();
        assert_eq!(first.statement.slot_index, 100);

        // Then slot 200
        let second = fetching.pop(u64::MAX).unwrap();
        assert_eq!(second.statement.slot_index, 200);

        // Then slot 300
        let third = fetching.pop(u64::MAX).unwrap();
        assert_eq!(third.statement.slot_index, 300);

        // No more
        assert!(fetching.pop(u64::MAX).is_none());
    }

    #[test]
    fn test_pop_respects_max_slot() {
        let fetching = FetchingEnvelopes::with_defaults();
        cache_test_quorum_set(&fetching);

        fetching.recv_envelope(make_test_envelope(100, 1));
        fetching.recv_envelope(make_test_envelope(200, 2));
        fetching.recv_envelope(make_test_envelope(300, 3));

        // Pop with max_slot=150 should only return slot 100
        let first = fetching.pop(150).unwrap();
        assert_eq!(first.statement.slot_index, 100);

        // Next pop with max_slot=150 should return None (200 and 300 are above)
        assert!(fetching.pop(150).is_none());
    }

    #[test]
    fn test_ready_slots_returns_sorted() {
        let fetching = FetchingEnvelopes::with_defaults();
        cache_test_quorum_set(&fetching);

        // Add in non-sequential order
        fetching.recv_envelope(make_test_envelope(300, 1));
        fetching.recv_envelope(make_test_envelope(100, 2));
        fetching.recv_envelope(make_test_envelope(200, 3));

        let slots = fetching.ready_slots();
        assert_eq!(slots, vec![100, 200, 300]);
    }

    // =========================================================================
    // Phase 2D: Broadcast on ready
    // =========================================================================

    #[test]
    fn test_broadcast_called_when_envelope_ready() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let fetching = FetchingEnvelopes::with_defaults();
        cache_test_quorum_set(&fetching);

        let broadcast_count = Arc::new(AtomicU64::new(0));
        let count_clone = broadcast_count.clone();
        fetching.set_broadcast(move |_env| {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });

        // recv_envelope should trigger broadcast when immediately ready
        fetching.recv_envelope(make_test_envelope(100, 1));
        assert_eq!(broadcast_count.load(Ordering::SeqCst), 1);

        // Second envelope also triggers broadcast
        fetching.recv_envelope(make_test_envelope(101, 2));
        assert_eq!(broadcast_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_broadcast_called_when_dependency_satisfied() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let fetching = FetchingEnvelopes::with_defaults();

        let broadcast_count = Arc::new(AtomicU64::new(0));
        let count_clone = broadcast_count.clone();
        fetching.set_broadcast(move |_env| {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });

        // Submit envelope that needs quorum set (not cached yet).
        // This starts tracking the quorum set hash in the fetcher.
        let envelope = make_test_envelope(100, 1);
        let qs_hash = FetchingEnvelopes::extract_quorum_set_hash(&envelope).unwrap();
        let result = fetching.recv_envelope(envelope);
        assert_eq!(result, RecvResult::Fetching);
        assert_eq!(broadcast_count.load(Ordering::SeqCst), 0);
        assert_eq!(fetching.fetching_count(), 1);
        assert_eq!(fetching.ready_count(), 0);

        // Deliver the quorum set via recv_quorum_set — this triggers the
        // fetcher→check_and_move_to_ready→broadcast path.
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        let received = fetching.recv_quorum_set(qs_hash, quorum_set);

        assert!(received, "recv_quorum_set should return true when tracked");
        assert_eq!(broadcast_count.load(Ordering::SeqCst), 1);
        assert_eq!(fetching.ready_count(), 1);
        assert!(
            fetching.pop(100).is_some(),
            "Envelope should be poppable after dependency satisfied"
        );
    }

    #[test]
    fn test_no_broadcast_when_no_callback() {
        let fetching = FetchingEnvelopes::with_defaults();
        cache_test_quorum_set(&fetching);

        // No broadcast set — should not panic
        fetching.recv_envelope(make_test_envelope(100, 1));
        assert_eq!(fetching.ready_count(), 1);
    }

    /// [AUDIT-XH3] Prepare statements must validate all ballot fields, not just
    /// the main ballot. The `prepared` and `prepared_prime` fields can contain
    /// different StellarValues that also need the STELLAR_VALUE_SIGNED check.
    #[test]
    fn test_audit_xh3_prepare_validates_prepared_and_prepared_prime() {
        use stellar_xdr::curr::{
            ScpBallot, ScpStatementPrepare, StellarValue, StellarValueExt, Value,
        };

        // Create a signed StellarValue for the main ballot
        let signed_sv = StellarValue {
            tx_set_hash: Hash([1u8; 32]),
            close_time: stellar_xdr::curr::TimePoint(1000),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Signed(stellar_xdr::curr::LedgerCloseValueSignature {
                node_id: XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                signature: Signature(vec![0u8; 64].try_into().unwrap()),
            }),
        };
        let signed_bytes = signed_sv.to_xdr(stellar_xdr::curr::Limits::none()).unwrap();

        // Create an unsigned (Basic) StellarValue for the prepared ballot
        let unsigned_sv = StellarValue {
            tx_set_hash: Hash([2u8; 32]),
            close_time: stellar_xdr::curr::TimePoint(999),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        };
        let unsigned_bytes = unsigned_sv
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap();

        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));

        // Prepare envelope: main ballot is signed, but prepared ballot is unsigned
        let envelope = ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: 100,
                pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                    quorum_set_hash: Hash([1u8; 32]),
                    ballot: ScpBallot {
                        counter: 1,
                        value: Value(signed_bytes.try_into().unwrap()),
                    },
                    prepared: Some(ScpBallot {
                        counter: 1,
                        value: Value(unsigned_bytes.try_into().unwrap()),
                    }),
                    prepared_prime: None,
                    n_c: 0,
                    n_h: 0,
                }),
            },
            signature: Signature(vec![0u8; 64].try_into().unwrap()),
        };

        // The envelope should be rejected because prepared has an unsigned value
        assert!(
            !FetchingEnvelopes::check_stellar_value_signed(&envelope),
            "Prepare with unsigned 'prepared' ballot should be rejected"
        );
    }

    /// Helper to create a signed StellarValue for test envelopes.
    fn make_test_stellar_value(tx_set_hash: [u8; 32]) -> Value {
        use stellar_xdr::curr::{
            LedgerCloseValueSignature, StellarValue, StellarValueExt, TimePoint, WriteXdr,
        };

        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let sv = StellarValue {
            tx_set_hash: Hash(tx_set_hash),
            close_time: TimePoint(100),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id,
                signature: Signature(vec![0u8; 64].try_into().unwrap()),
            }),
        };
        Value(sv.to_xdr(Limits::none()).unwrap().try_into().unwrap())
    }

    /// Regression test for #1117: NOMINATE envelopes must wait for their
    /// referenced tx_sets before being marked Ready.
    ///
    /// Before this fix, extract_tx_set_hash returned None for NOMINATE,
    /// so NOMINATE envelopes bypassed tx_set fetch gating entirely.
    #[test]
    fn test_nominate_waits_for_tx_sets() {
        let fetching = FetchingEnvelopes::with_defaults();
        cache_test_quorum_set(&fetching);

        // Create a NOMINATE envelope with two tx_set hashes in votes + accepted
        let value_a = make_test_stellar_value([0xAA; 32]);
        let value_b = make_test_stellar_value([0xBB; 32]);

        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let envelope = ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: 100,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash([1u8; 32]),
                    votes: vec![value_a].try_into().unwrap(),
                    accepted: vec![value_b].try_into().unwrap(),
                }),
            },
            signature: Signature(vec![0u8; 64].try_into().unwrap()),
        };

        // Without the tx_sets cached, NOMINATE should require fetching
        let result = fetching.recv_envelope(envelope);
        assert_eq!(
            result,
            RecvResult::Fetching,
            "NOMINATE with uncached tx_sets should return Fetching, not Ready"
        );
    }

    /// Regression test for #1117: NOMINATE is Ready when all tx_sets are cached.
    #[test]
    fn test_nominate_ready_when_tx_sets_cached() {
        let fetching = FetchingEnvelopes::with_defaults();
        cache_test_quorum_set(&fetching);

        let value_a = make_test_stellar_value([0xAA; 32]);

        // Pre-cache the tx_set
        fetching.cache_tx_set(Hash256::from_bytes([0xAA; 32]), 100, vec![1, 2, 3]);

        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let envelope = ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: 100,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash([1u8; 32]),
                    votes: vec![value_a].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: Signature(vec![0u8; 64].try_into().unwrap()),
        };

        let result = fetching.recv_envelope(envelope);
        assert_eq!(
            result,
            RecvResult::Ready,
            "NOMINATE with all tx_sets cached should be Ready"
        );
    }

    /// Regression test for AUDIT-132 (#1491): tx-set cache eviction must
    /// not be a no-op when all entries share the same slot. Before the fix,
    /// `evict_tx_set_cache_if_full` initialized `oldest_slot = current_slot`
    /// and used `<` comparison, so same-slot entries were never evicted.
    #[test]
    fn test_evict_tx_set_cache_same_slot_does_not_grow_unbounded() {
        let mut config = FetchingConfig::default();
        config.max_tx_set_cache = 4;
        let fetching = FetchingEnvelopes::new(config);

        let slot = 100;
        // Fill to the limit
        for i in 0..4u8 {
            let hash = Hash256::from_bytes([i; 32]);
            fetching.cache_tx_set(hash, slot, vec![i]);
        }
        assert_eq!(fetching.tx_set_cache_size(), 4);

        // Insert 4 more entries for the same slot — cache should stay at limit.
        for i in 4..8u8 {
            let hash = Hash256::from_bytes([i; 32]);
            fetching.cache_tx_set(hash, slot, vec![i]);
        }

        // Cache must not exceed the configured limit (previously grew to 8).
        assert!(
            fetching.tx_set_cache_size() <= 4,
            "tx_set_cache should not exceed max_tx_set_cache={}, got {}",
            4,
            fetching.tx_set_cache_size()
        );
    }

    // =========================================================================
    // Regression: DashMap iter + get_mut deadlock (#1719)
    // =========================================================================

    /// Build a signed StellarValue referencing a specific tx_set_hash.
    fn make_signed_stellar_value_with_hash(tx_set_hash: Hash256) -> Vec<u8> {
        use stellar_xdr::curr::{
            LedgerCloseValueSignature, StellarValue, StellarValueExt, TimePoint, VecM, WriteXdr,
        };
        let sv = StellarValue {
            tx_set_hash: Hash(tx_set_hash.0),
            close_time: TimePoint(12345),
            upgrades: VecM::default(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id: XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
            }),
        };
        sv.to_xdr(Limits::none()).unwrap()
    }

    /// Regression test for #1719: move_ready_envelopes_for_tx_set deadlocked
    /// because it called check_and_move_to_ready (which acquires a write lock
    /// via get_mut) while still holding a read guard from slots.iter().
    ///
    /// Uses a timeout channel to detect deadlock without hanging the test binary.
    #[test]
    fn test_tx_set_available_does_not_deadlock() {
        use std::sync::Arc;
        use std::time::Duration;

        let slot: SlotIndex = 100;
        let tx_set_hash = Hash256::from_bytes([42u8; 32]);
        let qs_hash = Hash256::from_bytes([1u8; 32]);

        let fetching = Arc::new(FetchingEnvelopes::with_defaults());

        // Cache the quorum set so the only missing dependency is the TxSet.
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        fetching.cache_quorum_set(
            qs_hash,
            ScpQuorumSet {
                threshold: 1,
                validators: vec![node_id.clone()].try_into().unwrap(),
                inner_sets: vec![].try_into().unwrap(),
            },
        );

        // Build an envelope whose StellarValue references our tx_set_hash.
        let sv_bytes = make_signed_stellar_value_with_hash(tx_set_hash);
        let envelope = ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash(qs_hash.0),
                    votes: vec![Value(sv_bytes.try_into().unwrap())]
                        .try_into()
                        .unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: Signature(vec![0u8; 64].try_into().unwrap()),
        };

        // Insert directly into slots.fetching, bypassing the fetcher tracker.
        // This ensures recv_tx_set (called inside tx_set_available) won't
        // handle it, forcing the fallback scan in move_ready_envelopes_for_tx_set.
        let env_hash = FetchingEnvelopes::compute_envelope_hash(&envelope);
        fetching
            .slots
            .entry(slot)
            .or_default()
            .fetching
            .insert(env_hash, (envelope, Instant::now()));

        assert_eq!(fetching.fetching_count(), 1);
        assert_eq!(fetching.ready_count(), 0);

        // Call tx_set_available on a separate thread with a timeout.
        // Before the fix, this deadlocked inside move_ready_envelopes_for_tx_set.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let fe = fetching.clone();
        std::thread::spawn(move || {
            fe.tx_set_available(tx_set_hash, slot);
            let _ = done_tx.send(());
        });

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("tx_set_available deadlocked (regression #1719)");

        // Envelope should have moved from fetching to ready.
        assert_eq!(
            fetching.fetching_count(),
            0,
            "envelope should no longer be in fetching"
        );
        assert_eq!(
            fetching.ready_count(),
            1,
            "envelope should have moved to ready"
        );
    }

    // =========================================================================
    // Regression: tx-set cache eviction strands fetching envelopes (#1954)
    // =========================================================================

    /// Helper: create an envelope referencing a specific tx-set hash and quorum-set hash.
    fn make_envelope_with_deps(
        slot: SlotIndex,
        node_seed: u8,
        tx_set_hash: Hash256,
        qs_hash: Hash256,
    ) -> ScpEnvelope {
        let sv_bytes = make_signed_stellar_value_with_hash(tx_set_hash);
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([node_seed; 32])));
        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash(qs_hash.0),
                    votes: vec![Value(sv_bytes.try_into().unwrap())]
                        .try_into()
                        .unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: Signature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    /// Helper: make a sane quorum set for testing.
    fn make_sane_quorum_set() -> ScpQuorumSet {
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        ScpQuorumSet {
            threshold: 1,
            validators: vec![node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        }
    }

    /// Regression test for #1954: tx-set eviction must not strand envelopes
    /// that already received the tx-set but are still waiting on a quorum-set.
    ///
    /// Scenario: envelope needs tx-set A + quorum-set Q. Deliver A (envelope
    /// stays fetching for Q). Dependency-aware eviction protects A. When Q
    /// arrives via cache_quorum_set + recheck, the envelope becomes ready.
    #[test]
    fn test_tx_set_eviction_does_not_strand_fetching_envelopes() {
        let mut config = FetchingConfig::default();
        config.max_tx_set_cache = 2;
        let fetching = FetchingEnvelopes::new(config);

        let tx_hash_a = Hash256::from_bytes([0xAA; 32]);
        let qs_hash = Hash256::from_bytes([0x01; 32]);

        // 1. Insert envelope needing tx-set A and quorum-set Q
        let envelope = make_envelope_with_deps(100, 1, tx_hash_a, qs_hash);
        let result = fetching.recv_envelope(envelope);
        assert_eq!(result, RecvResult::Fetching);
        assert_eq!(fetching.fetching_count(), 1);

        // 2. Deliver tx-set A — envelope stays fetching (Q still missing)
        fetching.recv_tx_set(tx_hash_a, 100, vec![1, 2, 3]);
        assert!(fetching.has_tx_set(&tx_hash_a));
        assert_eq!(fetching.fetching_count(), 1, "still waiting on quorum-set");

        // 3. Fill cache to trigger eviction — insert 2 more tx-sets
        let tx_hash_b = Hash256::from_bytes([0xBB; 32]);
        let tx_hash_c = Hash256::from_bytes([0xCC; 32]);
        fetching.cache_tx_set(tx_hash_b, 101, vec![4, 5, 6]);
        fetching.cache_tx_set(tx_hash_c, 102, vec![7, 8, 9]);

        // tx-set A should still be in cache (dependency-aware eviction protects it)
        assert!(
            fetching.has_tx_set(&tx_hash_a),
            "tx-set A should be protected from eviction while an envelope depends on it"
        );

        // 4. Deliver quorum-set Q — cache_quorum_set now auto-rechecks
        fetching.cache_quorum_set(qs_hash, make_sane_quorum_set());

        assert_eq!(
            fetching.ready_count(),
            1,
            "envelope should be ready after receiving both dependencies"
        );
        assert_eq!(
            fetching.fetching_count(),
            0,
            "no envelopes should remain in fetching"
        );
    }

    /// Regression test for #1954: quorum-set eviction must not strand envelopes.
    ///
    /// Mirror of the tx-set test: deliver quorum-set first, evict it via
    /// cache pressure, then deliver tx-set — envelope should still become ready.
    #[test]
    fn test_quorum_set_eviction_does_not_strand_fetching_envelopes() {
        let mut config = FetchingConfig::default();
        config.max_quorum_set_cache = 2;
        let fetching = FetchingEnvelopes::new(config);

        let tx_hash_a = Hash256::from_bytes([0xAA; 32]);
        let qs_hash = Hash256::from_bytes([0x01; 32]);

        // 1. Insert envelope needing tx-set A and quorum-set Q
        let envelope = make_envelope_with_deps(100, 1, tx_hash_a, qs_hash);
        let result = fetching.recv_envelope(envelope);
        assert_eq!(result, RecvResult::Fetching);

        // 2. Deliver quorum-set Q via direct cache
        fetching.cache_quorum_set(qs_hash, make_sane_quorum_set());
        assert!(fetching.has_quorum_set(&qs_hash));
        assert_eq!(fetching.fetching_count(), 1, "still waiting on tx-set");

        // 3. Fill quorum-set cache to trigger eviction
        let qs_hash_b = Hash256::from_bytes([0x02; 32]);
        let qs_hash_c = Hash256::from_bytes([0x03; 32]);
        fetching.cache_quorum_set(qs_hash_b, make_sane_quorum_set());
        fetching.cache_quorum_set(qs_hash_c, make_sane_quorum_set());

        // Q should still be in cache (dependency-aware eviction protects it)
        assert!(
            fetching.has_quorum_set(&qs_hash),
            "quorum-set Q should be protected from eviction while an envelope depends on it"
        );

        // 4. Deliver tx-set A and trigger recheck
        fetching.cache_tx_set(tx_hash_a, 100, vec![1, 2, 3]);
        fetching.move_ready_envelopes_for_tx_set(&tx_hash_a);

        assert_eq!(
            fetching.ready_count(),
            1,
            "envelope should be ready after both dependencies"
        );
    }

    /// Test that cache size remains bounded even when all entries are referenced
    /// by fetching envelopes (the fallback eviction path).
    #[test]
    fn test_tx_set_cache_bounded_when_all_referenced() {
        let mut config = FetchingConfig::default();
        config.max_tx_set_cache = 3;
        let fetching = FetchingEnvelopes::new(config);

        let qs_hash = Hash256::from_bytes([0x01; 32]);

        // Insert 3 envelopes, each referencing a different tx-set
        // (all will be fetching because quorum-set Q is missing)
        for i in 0..3u8 {
            let tx_hash = Hash256::from_bytes([0x10 + i; 32]);
            let envelope = make_envelope_with_deps(100 + i as u64, i + 1, tx_hash, qs_hash);
            let result = fetching.recv_envelope(envelope);
            assert_eq!(result, RecvResult::Fetching);

            // Deliver each tx-set so it's cached
            fetching.recv_tx_set(tx_hash, 100 + i as u64, vec![i]);
        }

        // All 3 tx-sets are cached and all referenced by fetching envelopes
        assert_eq!(fetching.tx_set_cache_size(), 3);
        assert_eq!(fetching.fetching_count(), 3);

        // Insert one more tx-set — cache should NOT exceed the limit
        let extra_hash = Hash256::from_bytes([0xFF; 32]);
        fetching.cache_tx_set(extra_hash, 200, vec![99]);

        assert!(
            fetching.tx_set_cache_size() <= 3,
            "tx_set_cache must not exceed limit, got {}",
            fetching.tx_set_cache_size()
        );
    }

    /// Test that check_and_move_to_ready re-starts fetching for evicted
    /// dependencies instead of silently stranding the envelope.
    ///
    /// This tests the recovery path: when all cache entries are referenced
    /// and one gets evicted, the next recheck re-registers the fetch so the
    /// dependency can be re-requested from peers.
    #[test]
    fn test_recheck_restarts_fetch_for_evicted_dependency() {
        let mut config = FetchingConfig::default();
        config.max_tx_set_cache = 2;
        let fetching = FetchingEnvelopes::new(config);

        let tx_hash_a = Hash256::from_bytes([0xAA; 32]);
        let tx_hash_b = Hash256::from_bytes([0xBB; 32]);
        let qs_hash_q = Hash256::from_bytes([0x01; 32]);
        let qs_hash_r = Hash256::from_bytes([0x02; 32]);

        // Insert two envelopes, each needing a different tx-set and quorum-set.
        // Both are fetching because their quorum-sets are missing.
        let env_a = make_envelope_with_deps(100, 1, tx_hash_a, qs_hash_q);
        let env_b = make_envelope_with_deps(101, 2, tx_hash_b, qs_hash_r);
        assert_eq!(fetching.recv_envelope(env_a), RecvResult::Fetching);
        assert_eq!(fetching.recv_envelope(env_b), RecvResult::Fetching);

        // Deliver both tx-sets (cache is now full at 2)
        fetching.recv_tx_set(tx_hash_a, 100, vec![1]);
        fetching.recv_tx_set(tx_hash_b, 101, vec![2]);
        assert_eq!(fetching.tx_set_cache_size(), 2);
        assert_eq!(fetching.fetching_count(), 2);

        // Force eviction by inserting a third tx-set. Since both entries are
        // referenced, the fallback evicts the oldest-slot entry.
        let tx_hash_extra = Hash256::from_bytes([0xCC; 32]);
        fetching.cache_tx_set(tx_hash_extra, 200, vec![3]);

        // One of the two referenced tx-sets was evicted
        let a_evicted = !fetching.has_tx_set(&tx_hash_a);
        let b_evicted = !fetching.has_tx_set(&tx_hash_b);
        assert!(
            a_evicted || b_evicted,
            "at least one referenced tx-set should have been evicted to maintain the bound"
        );

        // Deliver the quorum-set for the envelope whose tx-set was evicted.
        // check_and_move_to_ready will see the tx-set is missing and should
        // re-register the fetch (not silently strand the envelope).
        if a_evicted {
            // cache_quorum_set now auto-rechecks fetching envelopes
            fetching.cache_quorum_set(qs_hash_q, make_sane_quorum_set());

            // Envelope A should NOT be ready yet (tx-set A was evicted)
            assert_eq!(
                fetching.fetching_count(),
                2,
                "envelope A should still be fetching"
            );

            // But the fetch should have been re-started: verify by checking
            // the fetcher is now tracking the evicted hash again.
            assert!(
                fetching.tx_set_fetcher.is_tracking(&Hash(tx_hash_a.0)),
                "tx-set A fetch should have been re-started by check_and_move_to_ready"
            );

            // Simulate re-delivery of the evicted tx-set
            fetching.recv_tx_set(tx_hash_a, 100, vec![1]);
            assert_eq!(
                fetching.ready_count(),
                1,
                "envelope A should become ready after re-delivery"
            );
        } else {
            fetching.cache_quorum_set(qs_hash_r, make_sane_quorum_set());

            assert_eq!(
                fetching.fetching_count(),
                2,
                "envelope B should still be fetching"
            );
            assert!(
                fetching.tx_set_fetcher.is_tracking(&Hash(tx_hash_b.0)),
                "tx-set B fetch should have been re-started by check_and_move_to_ready"
            );

            fetching.recv_tx_set(tx_hash_b, 101, vec![2]);
            assert_eq!(
                fetching.ready_count(),
                1,
                "envelope B should become ready after re-delivery"
            );
        }
    }
}
