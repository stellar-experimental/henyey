//! Fetching envelopes management.
//!
//! This module handles SCP envelopes that are waiting for their dependencies
//! (TxSets and QuorumSets) to be fetched from peers. When an envelope arrives
//! referencing data we don't have, we start fetching it and queue the envelope.
//! Once all dependencies are received, the envelope is ready for processing.
//!
//! This is the Rust equivalent of stellar-core's `PendingEnvelopes` fetching logic.

use henyey_common::Hash256;
use henyey_crypto::RandomEvictionCache;
use henyey_overlay::{ItemFetcher, ItemFetcherConfig, ItemType, PeerId};
use henyey_scp::{is_quorum_set_sane, SlotIndex};
use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use stellar_xdr::{Hash, Limits, ReadXdr, ScpEnvelope, ScpQuorumSet};
use tracing::{debug, trace};

/// Callback type for requesting items from peers.
type AskPeerFn = Box<dyn Fn(&PeerId, &Hash, ItemType) + Send + Sync>;

/// Relay payload carrying timing metadata from receive to overlay broadcast.
pub struct ScpRelayEnvelope {
    pub envelope: ScpEnvelope,
    /// Overlay arrival time for receive-to-relay latency histogram (#2648).
    /// `None` for locally-originated envelopes.
    pub received_at: Option<Instant>,
    /// Which readiness path produced this relay.
    pub ready_path: ReadyPath,
}

/// Readiness path for an SCP envelope relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadyPath {
    /// Dependencies satisfied on first recv — no fetch wait.
    Immediate,
    /// Parked in fetching, moved to ready after deps arrived.
    Deferred,
}

/// Callback type for broadcasting ready envelopes to peers.
type BroadcastFn = Box<dyn Fn(ScpRelayEnvelope) + Send + Sync>;

/// Callback type for checking authoritative tx_set presence.
///
/// Queries the authoritative source (ScpDriver) to determine whether a
/// tx_set hash is known. This eliminates the need for a shadow cache.
/// Callback to check if a tx-set hash is cached. The `slot` argument allows
/// the callee to propagate max-last-seen-slot on cache touches (stellar-core
/// parity: `touchFetchCache` / `getKnownTxSet(..., slot, true)`).
type HasTxSetFn = Box<dyn Fn(&Hash256, u64) -> bool + Send + Sync>;

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
    /// Per-slot lifetime cap reached (defense-in-depth, henyey-specific).
    ///
    /// The slot already has [`FetchingConfig::max_envelopes_per_slot`]
    /// envelopes across all states. Not present in stellar-core.
    PerSlotFull,
    /// Future-slot budget exhausted (henyey-specific).
    ///
    /// The number of distinct future slots (slots > current_slot) has
    /// reached [`FetchingConfig::max_future_slots`]. Envelopes for
    /// *existing* future slots are still accepted; only new slot creation
    /// is blocked. Not present in stellar-core.
    FutureSlotsFull,
}

/// Entry for an envelope parked in the fetching state, awaiting dependencies.
struct FetchingEntry {
    envelope: ScpEnvelope,
    /// Overlay arrival time, carried from `PipelinedIntake::received_at`.
    /// `None` for non-overlay paths.
    received_at: Option<Instant>,
}

/// State of envelopes for a single slot.
#[derive(Default)]
pub struct SlotEnvelopes {
    /// Envelopes we have discarded.
    discarded: HashSet<Hash256>,
    /// Envelopes we have processed already.
    processed: HashSet<Hash256>,
    /// Envelopes we are fetching right now.
    fetching: HashMap<Hash256, FetchingEntry>,
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
    /// Maximum quorum sets to cache. Uses random-two-choice eviction (matching
    /// stellar-core's `RandomEvictionCache`). Must be at least 1.
    pub max_quorum_set_cache: usize,
    /// Per-slot envelope lifetime cap (defense-in-depth, henyey-specific).
    ///
    /// Total unique envelopes admitted to a slot across all states
    /// (fetching + ready + processed + discarded). Once this limit is reached,
    /// new envelopes return [`RecvResult::PerSlotFull`].
    ///
    /// Default: [`super::pending::MAX_ENVELOPES_PER_SLOT`] (5000). Set to 0
    /// to disable (useful for tests that need unrestricted insertion).
    ///
    /// Boundary: the Nth envelope is accepted; the (N+1)th is rejected.
    ///
    /// Not present in stellar-core. See `PARITY_STATUS.md`.
    pub max_envelopes_per_slot: usize,
    /// Maximum distinct future slots to buffer (henyey-specific).
    ///
    /// Limits the number of distinct slot keys with `slot > current_slot`.
    /// Envelopes for an existing future slot are always accepted; only
    /// creation of a *new* slot entry is blocked when the budget is full.
    ///
    /// Default: [`crate::sync_recovery::LEDGER_VALIDITY_BRACKET`] (100).
    /// Set to 0 to disable (useful for tests).
    pub max_future_slots: usize,
}

impl Default for FetchingConfig {
    fn default() -> Self {
        Self {
            tx_set_fetcher_config: ItemFetcherConfig::default(),
            quorum_set_fetcher_config: ItemFetcherConfig::default(),
            // Parity: stellar-core uses QSET_CACHE_SIZE = 10000
            // in PendingEnvelopes.cpp.
            max_quorum_set_cache: 10_000,
            max_envelopes_per_slot: super::pending::MAX_ENVELOPES_PER_SLOT,
            max_future_slots: crate::sync_recovery::LEDGER_VALIDITY_BRACKET as usize,
        }
    }
}

/// Manages fetching of TxSets and QuorumSets for SCP envelopes.
///
/// When an SCP envelope references a TxSet or QuorumSet we don't have,
/// this manager starts fetching it from peers. Once received, envelopes
/// waiting for that data become ready for processing.
pub struct FetchingEnvelopes {
    /// Per-slot envelope state.
    ///
    /// Uses `RwLock<BTreeMap>` (not `DashMap`) so that the future-slot
    /// admission check and slot insertion are atomic under the same write
    /// lock, eliminating the TOCTOU race described in #2520.
    /// BTreeMap provides ordered iteration matching stellar-core's
    /// `std::map` and is consistent with `PendingEnvelopes` (#2476).
    pub(crate) slots: RwLock<BTreeMap<SlotIndex, SlotEnvelopes>>,
    /// TxSet fetcher.
    tx_set_fetcher: ItemFetcher,
    /// QuorumSet fetcher.
    quorum_set_fetcher: ItemFetcher,
    /// Cached QuorumSets (hash -> data).
    ///
    /// Uses `RandomEvictionCache` with random-two-choice eviction, matching
    /// stellar-core's `mQsetCache` in `PendingEnvelopes`.
    quorum_set_cache: Mutex<RandomEvictionCache<Hash256, Arc<ScpQuorumSet>>>,
    /// Read-through callback for authoritative tx_set presence.
    ///
    /// Queries ScpDriver::has_tx_set to determine whether a tx_set is known.
    /// This replaces the former shadow `tx_set_cache` which could be poisoned
    /// if callers injected entries without authoritative acceptance (#2066).
    has_tx_set_fn: HasTxSetFn,
    /// Callback to broadcast envelopes when they become ready.
    ///
    /// Parity: stellar-core broadcasts envelopes to peers via
    /// `OverlayManager::broadcastMessage()` when dependencies are satisfied.
    broadcast: RwLock<Option<BroadcastFn>>,
    /// Statistics.
    stats: RwLock<FetchingStats>,
    /// Per-slot envelope lifetime cap. See [`FetchingConfig::max_envelopes_per_slot`].
    max_envelopes_per_slot: usize,
    /// Maximum distinct future slots to buffer. See [`FetchingConfig::max_future_slots`].
    max_future_slots: usize,
    /// Current slot index, updated by the herder lifecycle.
    /// Read with `Relaxed` ordering (no cross-atomic synchronization needed).
    current_slot: AtomicU64,
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
    /// Per-slot lifetime cap rejections (defense-in-depth).
    pub per_slot_full: u64,
    /// Envelopes dropped because all tracker caps were full (defense-in-depth).
    pub tracker_cap_drops: u64,
    /// Future-slot budget rejections.
    pub future_slots_full: u64,
}

impl FetchingEnvelopes {
    /// Create a new fetching envelopes manager.
    ///
    /// `has_tx_set_fn` is a callback that queries the authoritative source
    /// (ScpDriver) for tx_set presence. This eliminates the former shadow
    /// cache that could be poisoned (#2066).
    pub fn new(config: FetchingConfig, has_tx_set_fn: HasTxSetFn) -> Self {
        assert!(
            config.max_quorum_set_cache >= 1,
            "max_quorum_set_cache must be at least 1"
        );
        let qs_cache = RandomEvictionCache::new(config.max_quorum_set_cache);
        let max_envelopes_per_slot = config.max_envelopes_per_slot;
        let max_future_slots = config.max_future_slots;
        Self {
            tx_set_fetcher: ItemFetcher::new(
                ItemType::TxSet,
                config.tx_set_fetcher_config.clone(),
                None,
            ),
            quorum_set_fetcher: ItemFetcher::new(
                ItemType::QuorumSet,
                config.quorum_set_fetcher_config.clone(),
                None,
            ),
            slots: RwLock::new(BTreeMap::new()),
            quorum_set_cache: Mutex::new(qs_cache),
            has_tx_set_fn,
            broadcast: RwLock::new(None),
            stats: RwLock::new(FetchingStats::default()),
            max_envelopes_per_slot,
            max_future_slots,
            current_slot: AtomicU64::new(0),
        }
    }

    /// Create with default configuration and a given tx_set presence callback.
    pub fn with_defaults(has_tx_set_fn: HasTxSetFn) -> Self {
        Self::new(FetchingConfig::default(), has_tx_set_fn)
    }

    /// Update the current slot index for future-slot admission control.
    ///
    /// Called by the herder at bootstrap and on tracking-slot advance.
    pub fn set_current_slot(&self, slot: SlotIndex) {
        self.current_slot.store(slot, Ordering::Relaxed);
    }

    /// Read the current slot index.
    pub fn current_slot(&self) -> SlotIndex {
        self.current_slot.load(Ordering::Relaxed)
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
        F: Fn(ScpRelayEnvelope) + Send + Sync + 'static,
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

        self.stats.write().envelopes_received += 1;

        // Parity: reject envelopes with non-SIGNED StellarValues
        if !Self::check_stellar_value_signed(&envelope) {
            debug!(slot, "Rejecting envelope with non-SIGNED StellarValue");
            return RecvResult::Discarded;
        }

        self.recv_envelope_inner(envelope, None)
    }

    /// Receive a pre-validated SCP envelope with strict StellarValue signed check.
    ///
    /// Use this for envelopes that have already passed network-level validation
    /// (pre-filter, signature verification) and are entering FetchingEnvelopes
    /// for dependency tracking, relay, and slot-aware queuing. Despite prior
    /// validation, this method still enforces `STELLAR_VALUE_SIGNED` to prevent
    /// malformed or non-signed values from triggering fetch/relay side effects.
    ///
    /// Parity: stellar-core's `PendingEnvelopes::recvSCPEnvelope()` enforces
    /// strict `STELLAR_VALUE_SIGNED` validation before `startFetch` /
    /// `envelopeReady`. This method applies the same gate.
    pub fn recv_envelope_validated(
        &self,
        envelope: ScpEnvelope,
        received_at: Option<Instant>,
    ) -> RecvResult {
        self.stats.write().envelopes_received += 1;

        // Strict value validation: reject before any fetch/relay side effects.
        if !Self::check_stellar_value_signed(&envelope) {
            debug!(
                slot = envelope.statement.slot_index,
                "recv_envelope_validated: rejecting envelope with non-SIGNED StellarValue"
            );
            return RecvResult::Discarded;
        }

        self.recv_envelope_inner(envelope, received_at)
    }

    /// Inner implementation shared by recv_envelope and recv_envelope_validated.
    ///
    /// Two-phase design (#2520):
    /// Phase 1 (write lock): admission checks + state mutation
    /// Phase 2 (lock dropped): I/O operations (broadcast, fetch)
    fn recv_envelope_inner(
        &self,
        envelope: ScpEnvelope,
        received_at: Option<Instant>,
    ) -> RecvResult {
        let slot = envelope.statement.slot_index;
        let env_hash = Self::compute_envelope_hash(&envelope);

        // Phase 1: Under write lock — admission, dedup, deps, insert.
        enum Action {
            Broadcast(ScpEnvelope, Option<Instant>),
            Fetch {
                need_tx_set: bool,
                need_quorum_set: bool,
                envelope: ScpEnvelope,
                env_hash: Hash256,
            },
        }

        let (result, action) = {
            let mut slots = self.slots.write();

            // Future-slot admission control (atomic with insert, fixes #2520).
            // Only check when creating a NEW slot entry for a future slot.
            let current = self.current_slot.load(Ordering::Relaxed);
            if self.max_future_slots > 0 && slot > current && !slots.contains_key(&slot) {
                let future_count = slots.keys().filter(|&&k| k > current).count();
                if future_count >= self.max_future_slots {
                    self.stats.write().future_slots_full += 1;
                    return RecvResult::FutureSlotsFull;
                }
            }

            let slot_state = slots.entry(slot).or_default();

            // Check if already processed
            if slot_state.processed.contains(&env_hash) {
                self.stats.write().envelopes_duplicate += 1;
                return RecvResult::AlreadyProcessed;
            }

            // Check if previously discarded.
            // Parity: stellar-core's `isDiscarded()` (PendingEnvelopes.cpp:325-327)
            // returns ENVELOPE_STATUS_DISCARDED, distinct from the duplicate path.
            if slot_state.discarded.contains(&env_hash) {
                return RecvResult::Discarded;
            }

            // Check if already fetching
            if slot_state.fetching.contains_key(&env_hash) {
                return RecvResult::Fetching;
            }

            // Per-slot lifetime cap (defense-in-depth, henyey-specific).
            if self.max_envelopes_per_slot > 0 {
                let lifetime_count = slot_state.fetching.len()
                    + slot_state.ready.len()
                    + slot_state.processed.len()
                    + slot_state.discarded.len();
                if lifetime_count >= self.max_envelopes_per_slot {
                    self.stats.write().per_slot_full += 1;
                    return RecvResult::PerSlotFull;
                }
            }

            // Check if we have all dependencies.
            // Lock-order safety: check_dependencies calls qs_cache_exists
            // (Mutex<RandomEvictionCache>) and has_tx_set_fn (ScpDriver cache
            // lookup). Neither acquires the slots lock, so no deadlock. See
            // lock-order analysis in #2520.
            let (need_tx_set, need_quorum_set) = self.check_dependencies(&envelope);

            if !need_tx_set && !need_quorum_set {
                // All dependencies available — envelope is ready.
                slot_state.ready.push(envelope.clone());
                self.stats.write().envelopes_ready += 1;
                (RecvResult::Ready, Action::Broadcast(envelope, received_at))
            } else {
                // Park envelope — will be moved to ready when deps arrive.
                slot_state.fetching.insert(
                    env_hash,
                    FetchingEntry {
                        envelope: envelope.clone(),
                        received_at,
                    },
                );
                (
                    RecvResult::Fetching,
                    Action::Fetch {
                        need_tx_set,
                        need_quorum_set,
                        envelope,
                        env_hash,
                    },
                )
            }
        }; // write lock dropped here

        // Phase 2: I/O operations outside the lock.
        match action {
            Action::Broadcast(env, recv_at) => {
                self.broadcast_envelope(&env, recv_at, ReadyPath::Immediate);
            }
            Action::Fetch {
                need_tx_set,
                need_quorum_set,
                envelope,
                env_hash,
            } => {
                let mut any_tracked = false;

                if need_tx_set {
                    for tx_set_hash in Self::extract_tx_set_hashes(&envelope) {
                        if !self.is_tx_set_available(&tx_set_hash, envelope.statement.slot_index) {
                            let hash = Hash(tx_set_hash.0);
                            if self.tx_set_fetcher.fetch(hash, &envelope) {
                                any_tracked = true;
                            }
                        }
                    }
                }

                if need_quorum_set {
                    if let Some(qs_hash) = Self::extract_quorum_set_hash(&envelope) {
                        let hash = Hash(qs_hash.0);
                        if self.quorum_set_fetcher.fetch(hash, &envelope) {
                            any_tracked = true;
                        }
                    }
                }

                if !any_tracked {
                    // All trackers at capacity — remove from fetching since
                    // the envelope can't actually be tracked. Preserves the
                    // pre-#2520 behavior where untracked envelopes are dropped.
                    if let Some(s) = self.slots.write().get_mut(&slot) {
                        s.fetching.remove(&env_hash);
                    }
                    self.stats.write().tracker_cap_drops += 1;
                } else {
                    self.stats.write().envelopes_fetching += 1;
                }
            }
        }

        result
    }

    /// Receive a TxSet.
    ///
    /// Returns true if the TxSet was needed and envelopes may be ready.
    pub fn recv_tx_set(&self, hash: Hash256) -> bool {
        // Check if we're fetching this TxSet
        if !self.tx_set_fetcher.is_tracking(&Hash(hash.0)) {
            trace!("Received unrequested TxSet {}", hex::encode(hash.0));
            return false;
        }

        self.stats.write().tx_sets_received += 1;

        // Notify the fetcher and get waiting envelopes
        let waiting = self.tx_set_fetcher.recv(&Hash(hash.0)).unwrap_or_default();

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
            // Parity: stellar-core's discardSCPEnvelopesWithQSet() discards all
            // envelopes waiting on this quorum set and stops their fetchers.
            self.discard_envelopes_with_quorum_set(&hash);
            return false;
        }

        self.stats.write().quorum_sets_received += 1;

        // Cache the QuorumSet (eviction handled by RandomEvictionCache)
        self.qs_cache_put(hash, Arc::new(quorum_set));

        // Notify the fetcher and get waiting envelopes
        let waiting = self
            .quorum_set_fetcher
            .recv(&Hash(hash.0))
            .unwrap_or_default();

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
        let mut slots = self.slots.write();
        // BTreeMap iterates in order — no need for collect + sort.
        let ready_slot = slots
            .iter()
            .filter(|(&k, v)| k <= max_slot && !v.ready.is_empty())
            .map(|(&k, _)| k)
            .next();

        if let Some(slot) = ready_slot {
            if let Some(slot_state) = slots.get_mut(&slot) {
                if let Some(envelope) = slot_state.ready.pop() {
                    let env_hash = Self::compute_envelope_hash(&envelope);
                    slot_state.processed.insert(env_hash);
                    return Some(envelope);
                }
            }
        }

        None
    }

    /// Pop a ready envelope from exactly the given slot.
    ///
    /// Unlike `pop(max_slot)` which scans all slots ≤ max_slot, this only
    /// pops from the specified slot. Used when we need to remove a specific
    /// envelope we just inserted (e.g., after inline SCP processing in
    /// `process_scp_envelope`).
    pub fn pop_from_slot(&self, slot: SlotIndex) -> Option<ScpEnvelope> {
        let mut slots = self.slots.write();
        if let Some(slot_state) = slots.get_mut(&slot) {
            if let Some(envelope) = slot_state.ready.pop() {
                let env_hash = Self::compute_envelope_hash(&envelope);
                slot_state.processed.insert(env_hash);
                return Some(envelope);
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
        let mut slots = self.slots.write();
        slots.entry(slot).or_default().ready.extend(envelopes);
    }

    /// Get all ready slots in ascending order.
    ///
    /// Parity: stellar-core returns slots in sorted order since it uses std::map.
    pub fn ready_slots(&self) -> Vec<SlotIndex> {
        // BTreeMap iterates in order — no need for sort.
        self.slots
            .read()
            .iter()
            .filter(|(_, v)| !v.ready.is_empty())
            .map(|(&k, _)| k)
            .collect()
    }

    /// Count envelopes (fetching + ready) for a given slot.
    ///
    /// Used for admission control to prevent unbounded memory growth from
    /// future-slot floods.
    pub fn slot_envelope_count(&self, slot: SlotIndex) -> usize {
        self.slots
            .read()
            .get(&slot)
            .map(|s| s.fetching.len() + s.ready.len())
            .unwrap_or(0)
    }

    /// Count total unique envelopes ever admitted to a slot (lifetime count).
    ///
    /// Includes all states: fetching + ready + processed + discarded.
    /// Used for per-slot lifetime cap enforcement. Unlike
    /// [`slot_envelope_count`](Self::slot_envelope_count) which counts only
    /// active envelopes, this counts envelopes that have already been
    /// consumed, preventing the "immediate-pop bypass" where deps-satisfied
    /// envelopes drain the active queue but still grow `processed` without
    /// bound.
    pub fn slot_lifetime_count(&self, slot: SlotIndex) -> usize {
        self.slots
            .read()
            .get(&slot)
            .map(|s| s.fetching.len() + s.ready.len() + s.processed.len() + s.discarded.len())
            .unwrap_or(0)
    }

    /// Count total slots with envelopes above a given slot threshold.
    ///
    /// Used for admission control to limit the number of future slots
    /// buffered in the fetching pipeline.
    pub fn future_slot_count(&self, current_slot: SlotIndex) -> usize {
        self.slots
            .read()
            .keys()
            .filter(|&&k| k > current_slot)
            .count()
    }

    /// Erase data for slots outside the given range.
    ///
    /// Parity: stellar-core `PendingEnvelopes::eraseOutsideRange(min, max, slotToKeep)`.
    /// - `min_slot = Some(m)`: remove entries with key < m (except `slot_to_keep`)
    /// - `max_slot = Some(m)`: remove entries with key > m (except `slot_to_keep`)
    /// - `slot_to_keep` is exempt from both bounds
    pub fn erase_outside_range(
        &self,
        min_slot: Option<SlotIndex>,
        max_slot: Option<SlotIndex>,
        slot_to_keep: SlotIndex,
    ) {
        // Remove slot entries outside the valid range in a single write lock,
        // matching trim_stale's retain pattern.
        {
            let mut slots = self.slots.write();
            slots.retain(|&key, _| {
                if key == slot_to_keep {
                    return true;
                }
                let below_min = min_slot.is_some_and(|m| key < m);
                let above_max = max_slot.is_some_and(|m| key > m);
                !(below_min || above_max)
            });
        }

        // Tell fetchers to stop fetching for slots outside range
        self.tx_set_fetcher
            .stop_fetching_outside_range(min_slot, max_slot, slot_to_keep);
        self.quorum_set_fetcher
            .stop_fetching_outside_range(min_slot, max_slot, slot_to_keep);
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

    /// Estimate the heap footprint of the per-slot envelope buffers (#3845).
    ///
    /// O(number of buffered slots) — bounded by `max_future_slots` — reading
    /// only map/set/vec capacities. Envelope payloads themselves are carried by
    /// the SCP layer and are not double-counted here; only the inline
    /// `ScpEnvelope`/`FetchingEntry` struct footprints are included.
    pub fn estimate_heap_bytes(&self) -> usize {
        use henyey_common::memory::{
            btreemap_heap_bytes, hashmap_heap_bytes, hashset_heap_bytes, vec_heap_bytes,
        };
        let slots = self.slots.read();
        // Outer BTreeMap<SlotIndex, SlotEnvelopes>.
        let mut total = btreemap_heap_bytes(
            slots.len(),
            std::mem::size_of::<SlotIndex>(),
            std::mem::size_of::<SlotEnvelopes>(),
        );
        // Per-slot inner collections (same-module private field access).
        for slot in slots.values() {
            total += hashset_heap_bytes(slot.discarded.capacity(), std::mem::size_of::<Hash256>());
            total += hashset_heap_bytes(slot.processed.capacity(), std::mem::size_of::<Hash256>());
            total += hashmap_heap_bytes(
                slot.fetching.capacity(),
                std::mem::size_of::<Hash256>(),
                std::mem::size_of::<FetchingEntry>(),
            );
            total += vec_heap_bytes(slot.ready.capacity(), std::mem::size_of::<ScpEnvelope>());
        }
        total
    }

    /// Trim stale data while preserving state for slots after catchup.
    /// Called after catchup to release memory from stale data.
    pub fn trim_stale(&self, keep_after_slot: SlotIndex) {
        let initial_quorum_set_count = self.qs_cache_len();
        let initial_slots_count = self.slots.read().len();

        // Clear slots for old ledgers only
        self.slots.write().retain(|&slot, _| slot > keep_after_slot);

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

        let kept_slots = self.slots.read().len();

        tracing::info!(
            initial_quorum_set_count,
            initial_slots_count,
            kept_slots,
            keep_after_slot,
            "Trimmed stale fetching_envelopes caches"
        );
    }

    /// Get the number of cached QuorumSets.
    pub fn quorum_set_cache_size(&self) -> usize {
        self.qs_cache_len()
    }

    /// Get the number of slots being tracked.
    pub fn slots_count(&self) -> usize {
        self.slots.read().len()
    }

    /// Get the number of envelopes being fetched.
    pub fn fetching_count(&self) -> usize {
        self.slots.read().values().map(|v| v.fetching.len()).sum()
    }

    /// Get the number of ready envelopes.
    pub fn ready_count(&self) -> usize {
        self.slots.read().values().map(|v| v.ready.len()).sum()
    }

    /// Check if a slot has envelopes currently being fetched (waiting for tx_set).
    pub fn has_fetching_for_slot(&self, slot: SlotIndex) -> bool {
        self.slots
            .read()
            .get(&slot)
            .map(|s| !s.fetching.is_empty())
            .unwrap_or(false)
    }

    /// Check if we have a cached QuorumSet.
    pub fn has_quorum_set(&self, hash: &Hash256) -> bool {
        self.qs_cache_exists(hash)
    }

    /// Get a cached QuorumSet.
    pub fn get_quorum_set(&self, hash: &Hash256) -> Option<Arc<ScpQuorumSet>> {
        self.qs_cache_get(hash)
    }

    /// Signal that a tx_set is now authoritatively available.
    ///
    /// Re-evaluates all fetching envelopes that depend on this hash.
    /// Safe to call spuriously — if the read-through callback reports the
    /// hash as unavailable, envelopes remain in fetching.
    ///
    /// Also clears the ItemFetcher tracking for this hash so it stops
    /// requesting it from peers.
    pub fn on_tx_set_accepted(&self, hash: &Hash256) {
        // Stop the fetcher from re-requesting this hash and collect
        // any envelopes that were waiting on the fetcher path.
        let fetcher_waiting = self.tx_set_fetcher.recv(&Hash(hash.0)).unwrap_or_default();
        for env in fetcher_waiting {
            self.check_and_move_to_ready(env);
        }
        // Also check envelopes that weren't tracked through the fetcher
        // (e.g. envelopes routed through FetchingEnvelopes for quorum-set
        // dependency where the tx_set was already synced).
        self.move_ready_envelopes_for_tx_set(hash);
    }

    /// Check all fetching envelopes and move any that are now ready.
    fn move_ready_envelopes_for_tx_set(&self, tx_set_hash: &Hash256) {
        // Phase 1: Collect envelopes under read lock.
        let to_check: Vec<ScpEnvelope> = {
            let slots = self.slots.read();
            let mut result = Vec::new();
            for slot_state in slots.values() {
                for (_, entry) in slot_state.fetching.iter() {
                    if Self::extract_tx_set_hashes(&entry.envelope)
                        .iter()
                        .any(|h| h == tx_set_hash)
                    {
                        result.push(entry.envelope.clone());
                    }
                }
            }
            result
        }; // read lock dropped

        // Phase 2: Mutate with no lock held.
        for envelope in to_check {
            self.check_and_move_to_ready(envelope);
        }
    }

    /// Check all fetching envelopes referencing a quorum-set hash and move
    /// any that are now ready.
    fn move_ready_envelopes_for_quorum_set(&self, qs_hash: &Hash256) {
        let to_check: Vec<ScpEnvelope> = {
            let slots = self.slots.read();
            let mut result = Vec::new();
            for slot_state in slots.values() {
                for (_, entry) in slot_state.fetching.iter() {
                    if Self::extract_quorum_set_hash(&entry.envelope).as_ref() == Some(qs_hash) {
                        result.push(entry.envelope.clone());
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
    /// Parallel to `on_tx_set_accepted` — used when a quorum-set is received
    /// through means other than the quorum-set fetcher.
    pub fn cache_quorum_set(&self, hash: Hash256, quorum_set: ScpQuorumSet) {
        self.qs_cache_put(hash, Arc::new(quorum_set));

        // Check if any fetching envelopes are now ready
        self.move_ready_envelopes_for_quorum_set(&hash);
    }

    // --- Internal helpers ---

    // -- QuorumSet cache helpers (encapsulate Mutex<RandomEvictionCache>) --

    fn qs_cache_exists(&self, hash: &Hash256) -> bool {
        self.quorum_set_cache.lock().exists(hash)
    }

    fn qs_cache_get(&self, hash: &Hash256) -> Option<Arc<ScpQuorumSet>> {
        self.quorum_set_cache.lock().get(hash).cloned()
    }

    fn qs_cache_put(&self, hash: Hash256, value: Arc<ScpQuorumSet>) {
        self.quorum_set_cache.lock().put(hash, value);
    }

    fn qs_cache_len(&self) -> usize {
        self.quorum_set_cache.lock().len()
    }

    /// Check if a tx_set is available via the authoritative source.
    fn is_tx_set_available(&self, hash: &Hash256, slot: u64) -> bool {
        (self.has_tx_set_fn)(hash, slot)
    }
    /// Check what dependencies are missing for an envelope.
    ///
    /// Parity: checks both tx set and quorum set dependencies.
    /// An envelope is ready only when all referenced data is cached.
    fn check_dependencies(&self, envelope: &ScpEnvelope) -> (bool, bool) {
        let slot = envelope.statement.slot_index;
        let tx_set_hashes = Self::extract_tx_set_hashes(envelope);
        let need_tx_set = tx_set_hashes
            .iter()
            .any(|hash| !self.is_tx_set_available(hash, slot));

        let need_quorum_set = if let Some(hash) = Self::extract_quorum_set_hash(envelope) {
            !self.qs_cache_exists(&hash)
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
            // All dependencies available - move to ready.
            // Mutate under write lock, extract received_at, broadcast after dropping.
            let received_at = {
                let mut slots = self.slots.write();
                if let Some(slot_state) = slots.get_mut(&slot) {
                    if let Some(entry) = slot_state.fetching.remove(&env_hash) {
                        slot_state.ready.push(envelope.clone());
                        debug!(slot, "Envelope ready after receiving dependencies");
                        Some(entry.received_at)
                    } else {
                        // Already moved to ready by a prior check — no double-broadcast.
                        None
                    }
                } else {
                    None
                }
            };
            // Only broadcast on the actual fetching → ready transition (#2648).
            if let Some(recv_at) = received_at {
                self.broadcast_envelope(&envelope, recv_at, ReadyPath::Deferred);
            }
        } else {
            // Re-start fetching for any dependency that is not yet available.
            // Without this, the envelope would remain stranded in `fetching` forever.
            // Cap rejections are best-effort here — envelope stays in fetching
            // until temporal cleanup removes it.
            if need_tx_set {
                for tx_set_hash in Self::extract_tx_set_hashes(&envelope) {
                    if !self.is_tx_set_available(&tx_set_hash, slot) {
                        let hash = Hash(tx_set_hash.0);
                        let _ = self.tx_set_fetcher.fetch(hash, &envelope);
                        debug!(
                            slot,
                            tx_set = %hex::encode(tx_set_hash.0),
                            "Re-started fetch for missing tx-set dependency"
                        );
                    }
                }
            }
            if need_quorum_set {
                if let Some(qs_hash) = Self::extract_quorum_set_hash(&envelope) {
                    if !self.qs_cache_exists(&qs_hash) {
                        let hash = Hash(qs_hash.0);
                        let _ = self.quorum_set_fetcher.fetch(hash, &envelope);
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
    fn broadcast_envelope(
        &self,
        envelope: &ScpEnvelope,
        received_at: Option<Instant>,
        ready_path: ReadyPath,
    ) {
        if let Some(ref broadcast) = *self.broadcast.read() {
            broadcast(ScpRelayEnvelope {
                envelope: envelope.clone(),
                received_at,
                ready_path,
            });
        }
    }

    /// Stop fetching dependencies for an envelope.
    ///
    /// Parity: stellar-core's `PendingEnvelopes::stopFetch()` (PendingEnvelopes.cpp:615-629)
    /// removes the envelope from both the quorum-set and tx-set fetchers.
    fn stop_fetch(&self, envelope: &ScpEnvelope) {
        if let Some(qs_hash) = Self::extract_quorum_set_hash(envelope) {
            self.quorum_set_fetcher
                .stop_fetch(&Hash(qs_hash.0), envelope);
        }
        for tx_set_hash in Self::extract_tx_set_hashes(envelope) {
            self.tx_set_fetcher
                .stop_fetch(&Hash(tx_set_hash.0), envelope);
        }
    }

    /// Discard a single envelope: move from fetching to discarded and stop fetchers.
    ///
    /// Parity: stellar-core's `PendingEnvelopes::discardSCPEnvelope()`
    /// (PendingEnvelopes.cpp:398-423). Idempotent — returns immediately if the
    /// envelope is already in the discarded set.
    fn discard_envelope(&self, envelope: &ScpEnvelope) {
        let slot = envelope.statement.slot_index;
        let env_hash = Self::compute_envelope_hash(envelope);

        {
            let mut slots = self.slots.write();
            let slot_state = slots.entry(slot).or_default();

            // Idempotent: if already discarded, nothing to do (matches stellar-core's
            // early return when set::insert returns !r.second).
            if !slot_state.discarded.insert(env_hash) {
                return;
            }

            slot_state.fetching.remove(&env_hash);
        } // write lock dropped before I/O

        self.stop_fetch(envelope);

        debug!(slot, hash = %hex::encode(env_hash.0), "Discarded SCP envelope");
    }

    /// Discard all envelopes waiting on a given quorum set hash.
    ///
    /// Parity: stellar-core's `PendingEnvelopes::discardSCPEnvelopesWithQSet()`
    /// (PendingEnvelopes.cpp:149-158). Called when a received quorum set fails
    /// `isQuorumSetSane`.
    fn discard_envelopes_with_quorum_set(&self, qs_hash: &Hash256) {
        let envelopes = self.quorum_set_fetcher.fetching_for(&Hash(qs_hash.0));

        debug!(
            hash = %hex::encode(qs_hash.0),
            count = envelopes.len(),
            "Discarding SCP envelopes with insane QuorumSet"
        );

        for envelope in &envelopes {
            self.discard_envelope(envelope);
        }
    }

    /// Check that all StellarValues in an envelope have STELLAR_VALUE_SIGNED ext.
    ///
    /// Parity: stellar-core rejects envelopes containing non-signed StellarValues
    /// in both nomination (votes/accepted) and ballot statements.
    fn check_stellar_value_signed(envelope: &ScpEnvelope) -> bool {
        use stellar_xdr::{ScpStatementPledges, StellarValue, StellarValueExt};

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
        use stellar_xdr::StellarValue;

        henyey_scp::Slot::get_statement_values(&envelope.statement)
            .into_iter()
            .filter_map(|v| StellarValue::from_xdr(&v.0, Limits::none()).ok())
            .map(|sv| Hash256::from_bytes(sv.tx_set_hash.0))
            .collect()
    }

    /// Extract QuorumSet hash from an envelope.
    fn extract_quorum_set_hash(envelope: &ScpEnvelope) -> Option<Hash256> {
        use stellar_xdr::ScpStatementPledges;

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
    use stellar_xdr::{
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

    /// Short alias for tests that only need slot + unique node.
    fn make_envelope(slot: SlotIndex, node_seed: u8) -> ScpEnvelope {
        make_test_envelope(slot, node_seed)
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
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));

        // Without pre-caching the quorum set, nomination envelopes need fetching
        let envelope = make_test_envelope(100, 1);
        let result = fetching.recv_envelope(envelope);
        assert_eq!(result, RecvResult::Fetching);
    }

    #[test]
    fn test_recv_envelope_ready_when_cached() {
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));

        // Pre-cache the quorum set so envelope is immediately ready
        cache_test_quorum_set(&fetching);

        let envelope = make_test_envelope(100, 1);
        let result = fetching.recv_envelope(envelope);

        assert_eq!(result, RecvResult::Ready);
        assert_eq!(fetching.ready_count(), 1);
    }

    #[test]
    fn test_cache_quorum_set() {
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));

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
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
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
    fn test_erase_outside_range_min_only() {
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        cache_test_quorum_set(&fetching);

        // Add envelopes for different slots
        fetching.recv_envelope(make_test_envelope(100, 1));
        fetching.recv_envelope(make_test_envelope(101, 2));
        fetching.recv_envelope(make_test_envelope(102, 3));

        assert_eq!(fetching.ready_count(), 3);

        // Erase below 102, keeping 100 (min only, no max)
        fetching.erase_outside_range(Some(102), None, 100);

        // Slot 100 should be kept (slot_to_keep), 101 erased (below min), 102 kept (within range)
        let ready_slots = fetching.ready_slots();
        assert!(ready_slots.contains(&100));
        assert!(!ready_slots.contains(&101));
        assert!(ready_slots.contains(&102));
    }

    #[test]
    fn test_erase_outside_range_max_only() {
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        cache_test_quorum_set(&fetching);

        fetching.recv_envelope(make_test_envelope(100, 1));
        fetching.recv_envelope(make_test_envelope(101, 2));
        fetching.recv_envelope(make_test_envelope(200, 3));

        assert_eq!(fetching.ready_count(), 3);

        // Erase above 150 (max only), keeping slot 200 as slot_to_keep
        fetching.erase_outside_range(None, Some(150), 200);

        let ready_slots = fetching.ready_slots();
        assert!(ready_slots.contains(&100));
        assert!(ready_slots.contains(&101));
        assert!(ready_slots.contains(&200)); // kept as slot_to_keep
    }

    #[test]
    fn test_erase_outside_range_both_bounds() {
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        cache_test_quorum_set(&fetching);

        fetching.recv_envelope(make_test_envelope(50, 1));
        fetching.recv_envelope(make_test_envelope(100, 2));
        fetching.recv_envelope(make_test_envelope(150, 3));
        fetching.recv_envelope(make_test_envelope(200, 4));

        assert_eq!(fetching.ready_count(), 4);

        // Keep only slots in [100, 150], preserve slot 50 as slot_to_keep
        fetching.erase_outside_range(Some(100), Some(150), 50);

        let ready_slots = fetching.ready_slots();
        assert!(ready_slots.contains(&50)); // slot_to_keep
        assert!(ready_slots.contains(&100)); // within range
        assert!(ready_slots.contains(&150)); // within range
        assert!(!ready_slots.contains(&200)); // above max
    }

    #[test]
    fn test_erase_outside_range_none_none_is_noop() {
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        cache_test_quorum_set(&fetching);

        fetching.recv_envelope(make_test_envelope(100, 1));
        fetching.recv_envelope(make_test_envelope(200, 2));

        fetching.erase_outside_range(None, None, 0);

        assert_eq!(fetching.ready_count(), 2);
    }

    #[test]
    fn test_erase_outside_range_slot_to_keep_outside_range() {
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        cache_test_quorum_set(&fetching);

        fetching.recv_envelope(make_test_envelope(50, 1));
        fetching.recv_envelope(make_test_envelope(100, 2));
        fetching.recv_envelope(make_test_envelope(200, 3));

        // slot_to_keep = 200, which is above max_slot = 150
        fetching.erase_outside_range(Some(75), Some(150), 200);

        let ready_slots = fetching.ready_slots();
        assert!(!ready_slots.contains(&50)); // below min
        assert!(ready_slots.contains(&100)); // within range
        assert!(ready_slots.contains(&200)); // slot_to_keep even though above max
    }

    #[test]
    fn test_trim_stale_preserves_quorum_sets() {
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));

        // Add some quorum sets and slot state
        let qs_hash = Hash256::from_bytes([1u8; 32]);
        fetching.cache_quorum_set(
            qs_hash,
            ScpQuorumSet {
                threshold: 1,
                validators: vec![].try_into().unwrap(),
                inner_sets: vec![].try_into().unwrap(),
            },
        );

        // Add slot state for old and new slots
        fetching.slots.write().entry(98).or_default();
        fetching.slots.write().entry(101).or_default();

        // Trim with keep_after_slot = 100
        fetching.trim_stale(100);

        // Verify quorum_set_cache is preserved
        assert!(
            fetching.has_quorum_set(&qs_hash),
            "quorum sets should be preserved after trim"
        );

        // Old slot should be removed, new slot kept
        assert!(!fetching.slots.read().contains_key(&98));
        assert!(fetching.slots.read().contains_key(&101));
    }

    // =========================================================================
    // Phase 1A: Quorum Set Sanity Validation
    // =========================================================================

    #[test]
    fn test_recv_quorum_set_rejects_insane() {
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));

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
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
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
        use stellar_xdr::{StellarValue, StellarValueExt, TimePoint, VecM, WriteXdr};
        let sv = StellarValue {
            tx_set_hash: Hash([0u8; 32]),
            close_time: TimePoint(12345),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        };
        sv.to_xdr(Limits::none()).unwrap()
    }

    fn make_signed_stellar_value_bytes() -> Vec<u8> {
        use stellar_xdr::{
            LedgerCloseValueSignature, StellarValue, StellarValueExt, TimePoint, VecM, WriteXdr,
        };
        let sv = StellarValue {
            tx_set_hash: Hash([0u8; 32]),
            close_time: TimePoint(12345),
            upgrades: VecM::default(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id: XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                signature: stellar_xdr::Signature(vec![0u8; 64].try_into().unwrap()),
            }),
        };
        sv.to_xdr(Limits::none()).unwrap()
    }

    fn make_envelope_with_nomination_values(slot: SlotIndex, values: Vec<Vec<u8>>) -> ScpEnvelope {
        use stellar_xdr::Value;
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
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
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
        // Callback returns true for hash [0u8; 32] (the signed value's tx_set_hash)
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|hash, _| {
            *hash == Hash256::from_bytes([0u8; 32])
        }));
        cache_test_quorum_set(&fetching);

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
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
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
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
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
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
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
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
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

        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        cache_test_quorum_set(&fetching);

        let broadcast_count = Arc::new(AtomicU64::new(0));
        let count_clone = broadcast_count.clone();
        fetching.set_broadcast(move |_env| {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });

        // Immediate-ready path fires broadcast callback (parity: stellar-core
        // envelopeReady broadcasts when isFullyFetched is true).
        fetching.recv_envelope(make_test_envelope(100, 1));
        assert_eq!(broadcast_count.load(Ordering::SeqCst), 1);

        // Second envelope also triggers broadcast
        fetching.recv_envelope(make_test_envelope(101, 2));
        assert_eq!(broadcast_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_broadcast_called_when_dependency_satisfied() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));

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
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
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
        use stellar_xdr::{ScpBallot, ScpStatementPrepare, StellarValue, StellarValueExt, Value};

        // Create a signed StellarValue for the main ballot
        let signed_sv = StellarValue {
            tx_set_hash: Hash([1u8; 32]),
            close_time: stellar_xdr::TimePoint(1000),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Signed(stellar_xdr::LedgerCloseValueSignature {
                node_id: XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                signature: Signature(vec![0u8; 64].try_into().unwrap()),
            }),
        };
        let signed_bytes = signed_sv.to_xdr(stellar_xdr::Limits::none()).unwrap();

        // Create an unsigned (Basic) StellarValue for the prepared ballot
        let unsigned_sv = StellarValue {
            tx_set_hash: Hash([2u8; 32]),
            close_time: stellar_xdr::TimePoint(999),
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Basic,
        };
        let unsigned_bytes = unsigned_sv.to_xdr(stellar_xdr::Limits::none()).unwrap();

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
        use stellar_xdr::{
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
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
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
        // Callback returns true for the tx_set hash [0xAA; 32]
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|hash, _| {
            *hash == Hash256::from_bytes([0xAA; 32])
        }));
        cache_test_quorum_set(&fetching);

        let value_a = make_test_stellar_value([0xAA; 32]);

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

    // =========================================================================
    // Regression: DashMap iter + get_mut deadlock (#1719)
    // =========================================================================

    /// Build a signed StellarValue referencing a specific tx_set_hash.
    fn make_signed_stellar_value_with_hash(tx_set_hash: Hash256) -> Vec<u8> {
        use stellar_xdr::{
            LedgerCloseValueSignature, StellarValue, StellarValueExt, TimePoint, VecM, WriteXdr,
        };
        let sv = StellarValue {
            tx_set_hash: Hash(tx_set_hash.0),
            close_time: TimePoint(12345),
            upgrades: VecM::default(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id: XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                signature: stellar_xdr::Signature(vec![0u8; 64].try_into().unwrap()),
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
    fn test_on_tx_set_accepted_does_not_deadlock() {
        use std::sync::Arc;
        use std::time::Duration;

        let slot: SlotIndex = 100;
        let tx_set_hash = Hash256::from_bytes([42u8; 32]);
        let qs_hash = Hash256::from_bytes([1u8; 32]);

        // Create with a callback that reports the tx_set as available
        let target_hash = tx_set_hash;
        let fetching = Arc::new(FetchingEnvelopes::with_defaults(Box::new(
            move |hash, _| *hash == target_hash,
        )));

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
        // This ensures the fallback scan in move_ready_envelopes_for_tx_set is exercised.
        let env_hash = FetchingEnvelopes::compute_envelope_hash(&envelope);
        fetching
            .slots
            .write()
            .entry(slot)
            .or_default()
            .fetching
            .insert(
                env_hash,
                FetchingEntry {
                    envelope,
                    received_at: None,
                },
            );

        assert_eq!(fetching.fetching_count(), 1);
        assert_eq!(fetching.ready_count(), 0);

        // Call on_tx_set_accepted on a separate thread with a timeout.
        // Before the fix, this deadlocked inside move_ready_envelopes_for_tx_set.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let fe = fetching.clone();
        std::thread::spawn(move || {
            fe.on_tx_set_accepted(&tx_set_hash);
            let _ = done_tx.send(());
        });

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("on_tx_set_accepted deadlocked (regression #1719)");

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

    /// Test that on_tx_set_accepted is a no-op when the callback reports
    /// the hash as unavailable (envelope stays in fetching).
    #[test]
    fn test_on_tx_set_accepted_noop_when_not_in_driver() {
        let tx_set_hash = Hash256::from_bytes([0xAA; 32]);
        let qs_hash = Hash256::from_bytes([0x01; 32]);

        // Callback always returns false (tx_set not available)
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));

        // Cache quorum-set so only tx_set is missing
        fetching.cache_quorum_set(qs_hash, make_sane_quorum_set());

        // Insert envelope needing tx_set
        let envelope = make_envelope_with_deps(100, 1, tx_set_hash, qs_hash);
        let result = fetching.recv_envelope(envelope);
        assert_eq!(result, RecvResult::Fetching);
        assert_eq!(fetching.fetching_count(), 1);

        // Call on_tx_set_accepted — should be a no-op since callback returns false
        fetching.on_tx_set_accepted(&tx_set_hash);

        assert_eq!(
            fetching.fetching_count(),
            1,
            "envelope should stay in fetching when tx_set is not in driver"
        );
        assert_eq!(fetching.ready_count(), 0);
    }

    /// Test that on_tx_set_accepted moves envelopes to ready when the
    /// callback confirms the hash is available.
    #[test]
    fn test_on_tx_set_accepted_moves_to_ready() {
        let tx_set_hash = Hash256::from_bytes([0xAA; 32]);
        let qs_hash = Hash256::from_bytes([0x01; 32]);

        // Callback returns true for our target hash
        let target = tx_set_hash;
        let fetching = FetchingEnvelopes::with_defaults(Box::new(move |hash, _| *hash == target));

        // Cache quorum-set so only tx_set is missing
        fetching.cache_quorum_set(qs_hash, make_sane_quorum_set());

        // Insert envelope — initially the callback returns true but since
        // we need the envelope in fetching state for the test, insert directly
        let envelope = make_envelope_with_deps(100, 1, tx_set_hash, qs_hash);
        let env_hash = FetchingEnvelopes::compute_envelope_hash(&envelope);
        fetching
            .slots
            .write()
            .entry(100)
            .or_default()
            .fetching
            .insert(
                env_hash,
                FetchingEntry {
                    envelope,
                    received_at: None,
                },
            );
        assert_eq!(fetching.fetching_count(), 1);

        // on_tx_set_accepted should move it to ready
        fetching.on_tx_set_accepted(&tx_set_hash);

        assert_eq!(fetching.fetching_count(), 0);
        assert_eq!(fetching.ready_count(), 1);
    }

    /// Test: quorum-set eviction with re-fetch on recheck still works.
    #[test]
    fn test_quorum_set_eviction_does_not_strand_fetching_envelopes() {
        let mut config = FetchingConfig::default();
        config.max_quorum_set_cache = 2;

        let tx_hash_a = Hash256::from_bytes([0xAA; 32]);
        let qs_hash = Hash256::from_bytes([0x01; 32]);

        // tx_set is always available via the callback
        let target = tx_hash_a;
        let fetching = FetchingEnvelopes::new(config, Box::new(move |hash, _| *hash == target));

        // 1. Insert envelope needing tx-set A and quorum-set Q
        let envelope = make_envelope_with_deps(100, 1, tx_hash_a, qs_hash);
        let result = fetching.recv_envelope(envelope);
        // Since tx_set is available via callback, only quorum-set is missing
        assert_eq!(result, RecvResult::Fetching);

        // 2. Deliver quorum-set Q via direct cache
        fetching.cache_quorum_set(qs_hash, make_sane_quorum_set());
        assert!(fetching.has_quorum_set(&qs_hash));

        // Envelope should now be ready (both deps satisfied)
        assert_eq!(
            fetching.ready_count(),
            1,
            "envelope should be ready after both dependencies"
        );
    }

    /// Test: check_and_move_to_ready re-starts fetching for missing tx-sets.
    #[test]
    fn test_recheck_restarts_fetch_for_missing_dependency() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let tx_hash_a = Hash256::from_bytes([0xAA; 32]);
        let qs_hash = Hash256::from_bytes([0x01; 32]);

        // Initially tx_set is NOT available, then becomes available
        let available = Arc::new(AtomicBool::new(false));
        let avail_clone = available.clone();
        let target = tx_hash_a;
        let fetching = FetchingEnvelopes::with_defaults(Box::new(move |hash, _| {
            *hash == target && avail_clone.load(Ordering::Relaxed)
        }));

        // Cache quorum-set so only tx_set is the dependency
        fetching.cache_quorum_set(qs_hash, make_sane_quorum_set());

        // Insert envelope needing tx-set A
        let envelope = make_envelope_with_deps(100, 1, tx_hash_a, qs_hash);
        let result = fetching.recv_envelope(envelope);
        assert_eq!(result, RecvResult::Fetching);

        // on_tx_set_accepted with callback returning false: envelope stays fetching
        fetching.on_tx_set_accepted(&tx_hash_a);
        assert_eq!(fetching.fetching_count(), 1);

        // Now make the tx_set available and notify again
        available.store(true, Ordering::Relaxed);
        fetching.on_tx_set_accepted(&tx_hash_a);
        assert_eq!(fetching.ready_count(), 1);
    }

    /// Test: RandomEvictionCache enforces capacity bound.
    #[test]
    fn test_quorum_set_cache_enforces_capacity() {
        let mut config = FetchingConfig::default();
        config.max_quorum_set_cache = 3;

        let fetching = FetchingEnvelopes::new(config, Box::new(|_, _| false));

        // Insert 5 distinct quorum sets
        for i in 0..5u8 {
            let hash = Hash256::from_bytes([i; 32]);
            fetching.cache_quorum_set(hash, make_sane_quorum_set());
        }

        // Cache size must not exceed capacity
        assert!(
            fetching.quorum_set_cache_size() <= 3,
            "cache size {} exceeds capacity 3",
            fetching.quorum_set_cache_size()
        );
    }

    /// Test: when a quorum set is evicted, a new envelope needing it enters Fetching.
    #[test]
    fn test_evicted_quorum_set_triggers_refetch() {
        let mut config = FetchingConfig::default();
        // Capacity 1: any new insertion evicts the previous entry
        config.max_quorum_set_cache = 1;

        let tx_hash = Hash256::from_bytes([0xAA; 32]);
        let qs_hash = Hash256::from_bytes([0x01; 32]);

        // tx_set always available
        let target = tx_hash;
        let fetching = FetchingEnvelopes::new(config, Box::new(move |hash, _| *hash == target));

        // Deliver QS-A into cache
        fetching.cache_quorum_set(qs_hash, make_sane_quorum_set());
        assert!(fetching.has_quorum_set(&qs_hash));

        // Insert a different entry — with capacity 1, QS-A must be evicted
        let other_hash = Hash256::from_bytes([0x02; 32]);
        fetching.cache_quorum_set(other_hash, make_sane_quorum_set());

        // QS-A should now be evicted
        assert!(
            !fetching.has_quorum_set(&qs_hash),
            "QS-A should have been evicted"
        );

        // Insert an envelope that depends on QS-A — it should enter Fetching state
        let envelope = make_envelope_with_deps(100, 1, tx_hash, qs_hash);
        let result = fetching.recv_envelope(envelope);
        assert_eq!(
            result,
            RecvResult::Fetching,
            "envelope should be fetching since QS-A was evicted"
        );
    }

    /// Test: an already-fetching envelope whose quorum set is evicted
    /// re-starts fetching when check_and_move_to_ready runs.
    #[test]
    fn test_refetch_after_eviction_during_fetching() {
        let mut config = FetchingConfig::default();
        config.max_quorum_set_cache = 1;

        let tx_hash = Hash256::from_bytes([0xAA; 32]);
        let qs_hash = Hash256::from_bytes([0x01; 32]);

        // tx_set NOT available initially
        let available = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let avail_clone = available.clone();
        let target = tx_hash;
        let fetching = FetchingEnvelopes::new(
            config,
            Box::new(move |hash, _| {
                *hash == target && avail_clone.load(std::sync::atomic::Ordering::Relaxed)
            }),
        );

        // Insert envelope — needs both tx_set and quorum_set
        let envelope = make_envelope_with_deps(100, 1, tx_hash, qs_hash);
        let result = fetching.recv_envelope(envelope);
        assert_eq!(result, RecvResult::Fetching);

        // Deliver quorum-set so only tx_set is missing
        fetching.cache_quorum_set(qs_hash, make_sane_quorum_set());
        // Envelope stays fetching (tx_set still missing)
        assert_eq!(fetching.fetching_count(), 1);

        // Evict the quorum set by inserting a different one (capacity=1)
        let other_hash = Hash256::from_bytes([0x02; 32]);
        fetching.cache_quorum_set(other_hash, make_sane_quorum_set());
        assert!(!fetching.has_quorum_set(&qs_hash), "QS-A should be evicted");

        // Now make tx_set available and notify — this triggers check_and_move_to_ready
        available.store(true, std::sync::atomic::Ordering::Relaxed);
        fetching.on_tx_set_accepted(&tx_hash);

        // Envelope should still be fetching because QS was evicted
        // The re-fetch path should have been triggered
        assert_eq!(
            fetching.fetching_count(),
            1,
            "envelope should remain fetching until quorum set is re-delivered"
        );
        assert_eq!(fetching.ready_count(), 0);

        // Re-deliver the quorum set — now envelope should become ready
        fetching.cache_quorum_set(qs_hash, make_sane_quorum_set());
        // on_tx_set_accepted again to trigger recheck
        fetching.on_tx_set_accepted(&tx_hash);
        assert_eq!(
            fetching.ready_count(),
            1,
            "envelope should be ready after re-delivery of evicted quorum set"
        );
    }

    // --- Per-slot lifetime cap tests (issue #2411) ---

    /// Helper: create a FetchingEnvelopes with a low per-slot cap for testing.
    fn fetching_with_per_slot_cap(cap: usize) -> FetchingEnvelopes {
        let config = FetchingConfig {
            max_envelopes_per_slot: cap,
            ..Default::default()
        };
        // Quorum set NOT available so envelopes go to fetching state
        FetchingEnvelopes::new(config, Box::new(|_, _| false))
    }

    /// Helper: create FetchingEnvelopes with per-slot cap and pre-cached
    /// quorum set (envelopes go to ready state immediately).
    fn fetching_with_per_slot_cap_ready(cap: usize) -> FetchingEnvelopes {
        let config = FetchingConfig {
            max_envelopes_per_slot: cap,
            ..Default::default()
        };
        let fetching = FetchingEnvelopes::new(config, Box::new(|_, _| false));
        cache_test_quorum_set(&fetching);
        fetching
    }

    /// The per-slot lifetime cap is enforced: after `cap` envelopes, the next
    /// returns `PerSlotFull`.
    #[test]
    fn test_per_slot_lifetime_cap_enforced() {
        let fetching = fetching_with_per_slot_cap(3);

        // Fill 3 envelopes (the cap)
        for seed in 1..=3u8 {
            let result = fetching.recv_envelope(make_test_envelope(100, seed));
            assert_eq!(result, RecvResult::Fetching);
        }

        // 4th envelope exceeds the cap
        let result = fetching.recv_envelope(make_test_envelope(100, 4));
        assert_eq!(result, RecvResult::PerSlotFull);
    }

    /// Exactly `cap` envelopes are accepted.
    #[test]
    fn test_per_slot_cap_allows_up_to_limit() {
        let fetching = fetching_with_per_slot_cap(5);

        for seed in 1..=5u8 {
            let result = fetching.recv_envelope(make_test_envelope(100, seed));
            assert_eq!(
                result,
                RecvResult::Fetching,
                "envelope {seed} should be accepted"
            );
        }
    }

    /// Different slots have independent caps.
    #[test]
    fn test_per_slot_cap_independent_slots() {
        let fetching = fetching_with_per_slot_cap(2);

        // Fill slot 100
        assert_eq!(
            fetching.recv_envelope(make_test_envelope(100, 1)),
            RecvResult::Fetching
        );
        assert_eq!(
            fetching.recv_envelope(make_test_envelope(100, 2)),
            RecvResult::Fetching
        );
        assert_eq!(
            fetching.recv_envelope(make_test_envelope(100, 3)),
            RecvResult::PerSlotFull
        );

        // Slot 101 is independent
        assert_eq!(
            fetching.recv_envelope(make_test_envelope(101, 1)),
            RecvResult::Fetching
        );
        assert_eq!(
            fetching.recv_envelope(make_test_envelope(101, 2)),
            RecvResult::Fetching
        );
        assert_eq!(
            fetching.recv_envelope(make_test_envelope(101, 3)),
            RecvResult::PerSlotFull
        );
    }

    /// Envelopes that were popped (moved to processed state) still count
    /// against the cap. This is the key fix: prevents the "immediate-pop
    /// bypass" for current-slot envelopes.
    #[test]
    fn test_per_slot_cap_counts_processed() {
        let fetching = fetching_with_per_slot_cap_ready(3);

        // Insert 3 envelopes — they go to ready immediately (deps satisfied)
        for seed in 1..=3u8 {
            let result = fetching.recv_envelope(make_test_envelope(100, seed));
            assert_eq!(result, RecvResult::Ready);
        }

        // Pop all of them (moves to processed)
        for _ in 0..3 {
            let _ = fetching.pop(100);
        }

        // Lifetime count should be 3 (all processed), cap should fire
        assert_eq!(fetching.slot_lifetime_count(100), 3);
        let result = fetching.recv_envelope(make_test_envelope(100, 4));
        assert_eq!(result, RecvResult::PerSlotFull);
    }

    /// Duplicate envelopes get `AlreadyProcessed` (not PerSlotFull) because
    /// the dedup check runs before the cap check.
    #[test]
    fn test_per_slot_cap_dedup_before_cap() {
        let fetching = fetching_with_per_slot_cap(2);

        // Insert 2 envelopes (reaches cap)
        assert_eq!(
            fetching.recv_envelope(make_test_envelope(100, 1)),
            RecvResult::Fetching
        );
        assert_eq!(
            fetching.recv_envelope(make_test_envelope(100, 2)),
            RecvResult::Fetching
        );

        // Re-sending envelope 1 should get Fetching (already in fetching state)
        // since the fetching-dedup check is before the cap check
        let result = fetching.recv_envelope(make_test_envelope(100, 1));
        assert_eq!(result, RecvResult::Fetching);
    }

    /// Cap of 0 disables the limit (for unrestricted test scenarios).
    #[test]
    fn test_per_slot_cap_disabled_when_zero() {
        let fetching = fetching_with_per_slot_cap(0);

        // Should accept many envelopes without hitting cap
        for seed in 1..=100u8 {
            let result = fetching.recv_envelope(make_test_envelope(100, seed));
            assert_eq!(result, RecvResult::Fetching);
        }
    }

    /// The `per_slot_full` stat counter increments on rejection.
    #[test]
    fn test_per_slot_cap_stats() {
        let fetching = fetching_with_per_slot_cap(2);

        assert_eq!(
            fetching.recv_envelope(make_test_envelope(100, 1)),
            RecvResult::Fetching
        );
        assert_eq!(
            fetching.recv_envelope(make_test_envelope(100, 2)),
            RecvResult::Fetching
        );

        // These should be rejected
        assert_eq!(
            fetching.recv_envelope(make_test_envelope(100, 3)),
            RecvResult::PerSlotFull
        );
        assert_eq!(
            fetching.recv_envelope(make_test_envelope(100, 4)),
            RecvResult::PerSlotFull
        );

        assert_eq!(fetching.stats().per_slot_full, 2);
    }

    /// The `recv_envelope_validated` path also respects the cap.
    #[test]
    fn test_recv_envelope_validated_also_capped() {
        let fetching = fetching_with_per_slot_cap(2);

        assert_eq!(
            fetching.recv_envelope_validated(make_test_envelope(100, 1), None),
            RecvResult::Fetching
        );
        assert_eq!(
            fetching.recv_envelope_validated(make_test_envelope(100, 2), None),
            RecvResult::Fetching
        );
        assert_eq!(
            fetching.recv_envelope_validated(make_test_envelope(100, 3), None),
            RecvResult::PerSlotFull
        );
    }

    /// Regression test for the actual attack vector: current-slot
    /// deps-satisfied envelopes that get immediately popped. Without
    /// the lifetime cap, these bypass active-count checks.
    #[test]
    fn test_per_slot_cap_current_slot_regression() {
        let fetching = fetching_with_per_slot_cap_ready(3);

        // Simulate the attack: flood unique envelopes for same slot.
        // Each goes to ready and gets immediately popped.
        for seed in 1..=3u8 {
            let result = fetching.recv_envelope(make_test_envelope(100, seed));
            assert_eq!(result, RecvResult::Ready);
            let _ = fetching.pop(100); // Immediately consume
        }

        // The active count (fetching + ready) is now 0, but lifetime is 3.
        assert_eq!(fetching.slot_envelope_count(100), 0);
        assert_eq!(fetching.slot_lifetime_count(100), 3);

        // The 4th envelope should be rejected even though ready queue is empty.
        let result = fetching.recv_envelope(make_test_envelope(100, 4));
        assert_eq!(result, RecvResult::PerSlotFull);
    }

    /// `slot_lifetime_count` returns 0 for unknown slots.
    #[test]
    fn test_slot_lifetime_count_unknown_slot() {
        let fetching = fetching_with_per_slot_cap(100);
        assert_eq!(fetching.slot_lifetime_count(999), 0);
    }

    // --- Tracker cap drops tests (issue #2442) ---

    /// Test: when both fetchers are at max_trackers capacity, a new envelope
    /// needing novel hashes is dropped (not parked) and tracker_cap_drops is
    /// incremented.
    #[test]
    fn test_tracker_cap_drops_when_both_fetchers_full() {
        use henyey_overlay::ItemFetcherConfig;

        let config = FetchingConfig {
            tx_set_fetcher_config: ItemFetcherConfig {
                max_trackers: 2,
                ..Default::default()
            },
            quorum_set_fetcher_config: ItemFetcherConfig {
                max_trackers: 2,
                ..Default::default()
            },
            // Disable per-slot cap so it doesn't interfere.
            max_envelopes_per_slot: 0,
            ..Default::default()
        };

        // tx_set never available → tx_set fetching always needed.
        // Quorum sets not pre-cached → quorum_set fetching always needed.
        let fetching = FetchingEnvelopes::new(config, Box::new(|_, _| false));

        // Fill both fetchers with 2 distinct hashes each.
        let tx_hash_1 = Hash256::from_bytes([0xA1; 32]);
        let tx_hash_2 = Hash256::from_bytes([0xA2; 32]);
        let qs_hash_1 = Hash256::from_bytes([0xB1; 32]);
        let qs_hash_2 = Hash256::from_bytes([0xB2; 32]);

        let env1 = make_envelope_with_deps(100, 1, tx_hash_1, qs_hash_1);
        let env2 = make_envelope_with_deps(100, 2, tx_hash_2, qs_hash_2);

        assert_eq!(fetching.recv_envelope(env1), RecvResult::Fetching);
        assert_eq!(fetching.recv_envelope(env2), RecvResult::Fetching);

        // Verify both fetchers are at capacity.
        assert_eq!(fetching.tx_set_fetcher.num_trackers(), 2);
        assert_eq!(fetching.quorum_set_fetcher.num_trackers(), 2);
        assert_eq!(fetching.fetching_count(), 2);
        assert_eq!(fetching.stats.read().envelopes_fetching, 2);
        assert_eq!(fetching.stats.read().tracker_cap_drops, 0);

        // 3rd envelope with novel hashes — both fetchers reject.
        let tx_hash_3 = Hash256::from_bytes([0xA3; 32]);
        let qs_hash_3 = Hash256::from_bytes([0xB3; 32]);
        let env3 = make_envelope_with_deps(100, 3, tx_hash_3, qs_hash_3);

        let result = fetching.recv_envelope(env3);

        // Returns Fetching (indistinguishable from normal fetching at API level).
        assert_eq!(result, RecvResult::Fetching);
        // Envelope was NOT parked — fetching count unchanged.
        assert_eq!(fetching.fetching_count(), 2);
        assert_eq!(fetching.stats.read().envelopes_fetching, 2);
        // tracker_cap_drops incremented.
        assert_eq!(fetching.stats.read().tracker_cap_drops, 1);
    }

    // --- Tests for discard_envelope / discard_envelopes_with_quorum_set ---

    #[test]
    fn test_discard_insane_qset_moves_to_discarded() {
        // Verify that receiving an insane quorum set moves waiting envelopes
        // from fetching to discarded.
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        let qs_hash = Hash256::from_bytes([42u8; 32]);

        // Submit an envelope that needs this quorum set
        let envelope = make_envelope_with_deps(100, 1, Hash256::from_bytes([0xAA; 32]), qs_hash);
        assert_eq!(fetching.recv_envelope(envelope), RecvResult::Fetching);
        assert_eq!(fetching.fetching_count(), 1);

        // Receive an insane quorum set
        let insane_qs = ScpQuorumSet {
            threshold: 5,
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        fetching.recv_quorum_set(qs_hash, insane_qs);

        // Envelope should be removed from fetching and placed in discarded
        assert_eq!(fetching.fetching_count(), 0);
        let slot_state = fetching.slots.read();
        let slot = slot_state.get(&100).unwrap();
        assert_eq!(slot.discarded.len(), 1);
        assert_eq!(slot.fetching.len(), 0);
    }

    #[test]
    fn test_discard_insane_qset_multiple_slots() {
        // Verify discarding works across multiple slots with envelopes waiting
        // on the same quorum set hash.
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        let qs_hash = Hash256::from_bytes([42u8; 32]);
        let tx_hash = Hash256::from_bytes([0xAA; 32]);

        // Submit envelopes in different slots referencing the same qset
        let env1 = make_envelope_with_deps(100, 1, tx_hash, qs_hash);
        let env2 = make_envelope_with_deps(200, 2, tx_hash, qs_hash);
        let env3 = make_envelope_with_deps(300, 3, tx_hash, qs_hash);

        assert_eq!(fetching.recv_envelope(env1), RecvResult::Fetching);
        assert_eq!(fetching.recv_envelope(env2), RecvResult::Fetching);
        assert_eq!(fetching.recv_envelope(env3), RecvResult::Fetching);
        assert_eq!(fetching.fetching_count(), 3);

        // Receive insane qset — all envelopes should be discarded
        let insane_qs = ScpQuorumSet {
            threshold: 5,
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        fetching.recv_quorum_set(qs_hash, insane_qs);

        assert_eq!(fetching.fetching_count(), 0);
        for slot in [100, 200, 300] {
            let slots = fetching.slots.read();
            let slot_state = slots.get(&slot).unwrap();
            assert_eq!(slot_state.discarded.len(), 1);
            assert_eq!(slot_state.fetching.len(), 0);
        }
    }

    #[test]
    fn test_discarded_envelope_not_revived_by_txset() {
        // Verify that a discarded envelope is not moved to ready when its
        // tx_set later arrives — both fetchers must be cleaned up.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let tx_available = Arc::new(AtomicBool::new(false));
        let tx_available_clone = tx_available.clone();

        let fetching = FetchingEnvelopes::with_defaults(Box::new(move |_, _| {
            tx_available_clone.load(Ordering::Relaxed)
        }));

        let qs_hash = Hash256::from_bytes([42u8; 32]);
        let tx_hash = Hash256::from_bytes([0xAA; 32]);

        // Submit envelope needing both qset and txset
        let envelope = make_envelope_with_deps(100, 1, tx_hash, qs_hash);
        assert_eq!(fetching.recv_envelope(envelope), RecvResult::Fetching);

        // Discard via insane qset
        let insane_qs = ScpQuorumSet {
            threshold: 5,
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        fetching.recv_quorum_set(qs_hash, insane_qs);
        assert_eq!(fetching.fetching_count(), 0);

        // Now simulate tx_set becoming available
        tx_available.store(true, Ordering::Relaxed);
        fetching.on_tx_set_accepted(&tx_hash);

        // Envelope should NOT be revived — it was discarded
        assert_eq!(fetching.ready_count(), 0);
        assert_eq!(fetching.fetching_count(), 0);

        // tx_set fetcher should not be tracking the hash anymore
        assert!(
            !fetching.tx_set_fetcher.is_tracking(&Hash(tx_hash.0)),
            "tx_set fetcher should be cleaned up after discard"
        );
    }

    #[test]
    fn test_re_receive_discarded_envelope_returns_discarded() {
        // Verify that re-receiving a discarded envelope returns Discarded,
        // not AlreadyProcessed.
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        let qs_hash = Hash256::from_bytes([42u8; 32]);
        let tx_hash = Hash256::from_bytes([0xAA; 32]);

        let envelope = make_envelope_with_deps(100, 1, tx_hash, qs_hash);
        assert_eq!(
            fetching.recv_envelope(envelope.clone()),
            RecvResult::Fetching
        );

        // Discard via insane qset
        let insane_qs = ScpQuorumSet {
            threshold: 5,
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        fetching.recv_quorum_set(qs_hash, insane_qs);

        // Re-receive the same envelope — should return Discarded
        let result = fetching.recv_envelope(envelope);
        assert_eq!(result, RecvResult::Discarded);
    }

    #[test]
    fn test_discard_increases_slot_lifetime_count() {
        // Verify that discarded envelopes contribute to slot_lifetime_count.
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        let qs_hash = Hash256::from_bytes([42u8; 32]);
        let tx_hash = Hash256::from_bytes([0xAA; 32]);

        let envelope = make_envelope_with_deps(100, 1, tx_hash, qs_hash);
        assert_eq!(fetching.recv_envelope(envelope), RecvResult::Fetching);

        // Lifetime count = 1 (in fetching)
        assert_eq!(fetching.slot_lifetime_count(100), 1);

        // Discard via insane qset
        let insane_qs = ScpQuorumSet {
            threshold: 5,
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        fetching.recv_quorum_set(qs_hash, insane_qs);

        // Lifetime count should still be 1 (moved from fetching to discarded)
        assert_eq!(fetching.slot_lifetime_count(100), 1);

        // The envelope is in discarded, not fetching
        let slots = fetching.slots.read();
        let slot_state = slots.get(&100).unwrap();
        assert_eq!(slot_state.fetching.len(), 0);
        assert_eq!(slot_state.discarded.len(), 1);
    }

    #[test]
    fn test_discard_is_idempotent() {
        // Verify that discarding the same envelope twice is a no-op.
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        let qs_hash = Hash256::from_bytes([42u8; 32]);
        let tx_hash = Hash256::from_bytes([0xAA; 32]);

        let envelope = make_envelope_with_deps(100, 1, tx_hash, qs_hash);
        assert_eq!(
            fetching.recv_envelope(envelope.clone()),
            RecvResult::Fetching
        );

        // Manually discard twice
        fetching.discard_envelope(&envelope);
        fetching.discard_envelope(&envelope);

        // Only one entry in discarded
        let slots = fetching.slots.read();
        let slot_state = slots.get(&100).unwrap();
        assert_eq!(slot_state.discarded.len(), 1);
    }

    #[test]
    fn test_discard_cleans_up_fetcher_trackers() {
        // Verify that both quorum-set and tx-set fetcher trackers are cleaned up.
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        let qs_hash = Hash256::from_bytes([42u8; 32]);
        let tx_hash = Hash256::from_bytes([0xAA; 32]);

        let envelope = make_envelope_with_deps(100, 1, tx_hash, qs_hash);
        assert_eq!(fetching.recv_envelope(envelope), RecvResult::Fetching);

        // Both fetchers should be tracking
        assert!(fetching.quorum_set_fetcher.is_tracking(&Hash(qs_hash.0)));
        assert!(fetching.tx_set_fetcher.is_tracking(&Hash(tx_hash.0)));

        // Discard via insane qset
        let insane_qs = ScpQuorumSet {
            threshold: 5,
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        fetching.recv_quorum_set(qs_hash, insane_qs);

        // Both fetchers should have their trackers cleaned up
        assert!(
            !fetching.quorum_set_fetcher.is_tracking(&Hash(qs_hash.0)),
            "quorum_set fetcher should be cleaned up after discard"
        );
        assert!(
            !fetching.tx_set_fetcher.is_tracking(&Hash(tx_hash.0)),
            "tx_set fetcher should be cleaned up after discard"
        );
    }

    // ── #2520 regression: future-slot admission (TOCTOU fix) ──

    #[test]
    fn test_future_slot_admission_rejects_when_budget_full() {
        // Verify that FetchingEnvelopes enforces max_future_slots internally.
        let config = FetchingConfig {
            max_future_slots: 3,
            max_envelopes_per_slot: 0,
            ..Default::default()
        };
        let fetching = FetchingEnvelopes::new(config, Box::new(|_, _| false));
        fetching.set_current_slot(100);

        // Fill 3 future slots
        for s in [101, 102, 103] {
            let env = make_envelope(s, 1);
            assert_eq!(fetching.recv_envelope(env), RecvResult::Fetching);
        }
        assert_eq!(fetching.future_slot_count(100), 3);

        // 4th new future slot is rejected
        let env = make_envelope(104, 1);
        assert_eq!(fetching.recv_envelope(env), RecvResult::FutureSlotsFull);
        assert_eq!(fetching.slots_count(), 3); // no new slot created
    }

    #[test]
    fn test_future_slot_admission_existing_slot_bypass() {
        // Envelopes targeting an existing future slot must still be accepted
        // even when the future-slot budget is full (#2520).
        let config = FetchingConfig {
            max_future_slots: 2,
            max_envelopes_per_slot: 0,
            ..Default::default()
        };
        let fetching = FetchingEnvelopes::new(config, Box::new(|_, _| false));
        fetching.set_current_slot(100);

        // Fill 2 future slots
        let env1 = make_envelope(101, 1);
        let env2 = make_envelope(102, 1);
        assert_eq!(fetching.recv_envelope(env1), RecvResult::Fetching);
        assert_eq!(fetching.recv_envelope(env2), RecvResult::Fetching);

        // Budget is full, but a second envelope for slot 101 is allowed
        let env3 = make_envelope(101, 2);
        assert_eq!(fetching.recv_envelope(env3), RecvResult::Fetching);
        assert_eq!(fetching.slot_envelope_count(101), 2);
    }

    #[test]
    fn test_future_slot_admission_current_and_past_always_accepted() {
        // Current-slot and past-slot envelopes bypass future-slot admission.
        let config = FetchingConfig {
            max_future_slots: 1,
            max_envelopes_per_slot: 0,
            ..Default::default()
        };
        let fetching = FetchingEnvelopes::new(config, Box::new(|_, _| false));
        fetching.set_current_slot(100);

        // Fill the single future-slot budget
        let env = make_envelope(101, 1);
        assert_eq!(fetching.recv_envelope(env), RecvResult::Fetching);

        // Current slot (100) and past slot (99) are unaffected
        let env_current = make_envelope(100, 1);
        let env_past = make_envelope(99, 1);
        assert_eq!(fetching.recv_envelope(env_current), RecvResult::Fetching);
        assert_eq!(fetching.recv_envelope(env_past), RecvResult::Fetching);
    }

    #[test]
    fn test_future_slot_admission_stats_tracked() {
        // Verify that FutureSlotsFull rejections increment the stat counter.
        let config = FetchingConfig {
            max_future_slots: 1,
            max_envelopes_per_slot: 0,
            ..Default::default()
        };
        let fetching = FetchingEnvelopes::new(config, Box::new(|_, _| false));
        fetching.set_current_slot(100);

        let env1 = make_envelope(101, 1);
        assert_eq!(fetching.recv_envelope(env1), RecvResult::Fetching);

        let env2 = make_envelope(102, 1);
        assert_eq!(fetching.recv_envelope(env2), RecvResult::FutureSlotsFull);
        let env3 = make_envelope(103, 1);
        assert_eq!(fetching.recv_envelope(env3), RecvResult::FutureSlotsFull);

        let stats = fetching.stats.read();
        assert_eq!(stats.future_slots_full, 2);
    }

    #[test]
    fn test_future_slot_admission_disabled_when_zero() {
        // max_future_slots=0 disables the check — unlimited future slots.
        let config = FetchingConfig {
            max_future_slots: 0,
            max_envelopes_per_slot: 0,
            ..Default::default()
        };
        let fetching = FetchingEnvelopes::new(config, Box::new(|_, _| false));
        fetching.set_current_slot(100);

        for s in 101..=300 {
            let env = make_envelope(s, 1);
            assert_eq!(fetching.recv_envelope(env), RecvResult::Fetching);
        }
        assert_eq!(fetching.slots_count(), 200);
    }

    #[test]
    fn test_future_slot_admission_concurrent_race() {
        // Regression test for the TOCTOU race (#2520): spawn threads that
        // concurrently attempt to create new future slots. Without the
        // RwLock fix, more than max_future_slots could sneak in.
        use std::sync::{Arc, Barrier};

        let config = FetchingConfig {
            max_future_slots: 5,
            max_envelopes_per_slot: 0,
            ..Default::default()
        };
        let fetching = Arc::new(FetchingEnvelopes::new(config, Box::new(|_, _| false)));
        fetching.set_current_slot(100);

        let barrier = Arc::new(Barrier::new(20));
        let mut handles = Vec::new();

        for i in 0..20u64 {
            let fe = Arc::clone(&fetching);
            let b = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                b.wait();
                let env = make_envelope(101 + i, 1);
                fe.recv_envelope(env)
            }));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let accepted = results
            .iter()
            .filter(|r| matches!(r, RecvResult::Fetching))
            .count();
        let rejected = results
            .iter()
            .filter(|r| matches!(r, RecvResult::FutureSlotsFull))
            .count();

        assert_eq!(accepted, 5, "exactly max_future_slots should be accepted");
        assert_eq!(rejected, 15, "the rest should be rejected");
        assert_eq!(
            fetching.slots_count(),
            5,
            "no more than max_future_slots slot entries"
        );
    }

    // =========================================================================
    // Issue #2648: ScpRelayEnvelope metadata tests
    // =========================================================================

    #[test]
    fn test_broadcast_callback_immediate_ready() {
        use std::sync::Mutex;

        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        cache_test_quorum_set(&fetching);

        let captured = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        fetching.set_broadcast(move |relay_env| {
            cap.lock()
                .unwrap()
                .push((relay_env.received_at, relay_env.ready_path));
        });

        // Overlay path with received_at set.
        let t0 = Instant::now();
        fetching.recv_envelope_validated(make_test_envelope(100, 1), Some(t0));

        let relays = captured.lock().unwrap();
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0].0, Some(t0));
        assert_eq!(relays[0].1, ReadyPath::Immediate);
    }

    #[test]
    fn test_broadcast_callback_deferred_ready() {
        use std::sync::Mutex;

        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));

        let captured = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        fetching.set_broadcast(move |relay_env| {
            cap.lock()
                .unwrap()
                .push((relay_env.received_at, relay_env.ready_path));
        });

        // Submit envelope that needs quorum set (deferred path).
        let t0 = Instant::now();
        let envelope = make_test_envelope(100, 1);
        let qs_hash = FetchingEnvelopes::extract_quorum_set_hash(&envelope).unwrap();
        let result = fetching.recv_envelope_validated(envelope, Some(t0));
        assert_eq!(result, RecvResult::Fetching);
        assert!(captured.lock().unwrap().is_empty());

        // Deliver dep → triggers deferred broadcast.
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        fetching.recv_quorum_set(qs_hash, quorum_set);

        let relays = captured.lock().unwrap();
        assert_eq!(relays.len(), 1);
        assert_eq!(relays[0].0, Some(t0));
        assert_eq!(relays[0].1, ReadyPath::Deferred);
    }

    #[test]
    fn test_broadcast_callback_local_path() {
        use std::sync::Mutex;

        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        cache_test_quorum_set(&fetching);

        let captured = Arc::new(Mutex::new(Vec::new()));
        let cap = captured.clone();
        fetching.set_broadcast(move |relay_env| {
            cap.lock().unwrap().push(relay_env.received_at);
        });

        // Local path: received_at is None.
        fetching.recv_envelope_validated(make_test_envelope(100, 1), None);

        let relays = captured.lock().unwrap();
        assert_eq!(relays.len(), 1);
        assert!(
            relays[0].is_none(),
            "local path should have received_at=None"
        );
    }

    #[test]
    fn test_no_double_broadcast_on_multiple_dep_arrivals() {
        use std::sync::atomic::{AtomicU64, Ordering};

        // Envelope needs only quorum set (tx_set check uses `has_authoritative_tx_set`,
        // which defaults to `|_| false`, but that means tx_set is "always needed" — so
        // use a callback that says "always available" and test double-broadcast via
        // quorum set delivery).
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| true));

        let broadcast_count = Arc::new(AtomicU64::new(0));
        let count_clone = broadcast_count.clone();
        fetching.set_broadcast(move |_env| {
            count_clone.fetch_add(1, Ordering::SeqCst);
        });

        // Envelope needing quorum set only (not cached).
        let envelope = make_test_envelope(100, 1);
        let qs_hash = FetchingEnvelopes::extract_quorum_set_hash(&envelope).unwrap();
        let result = fetching.recv_envelope_validated(envelope, Some(Instant::now()));
        assert_eq!(result, RecvResult::Fetching);
        assert_eq!(broadcast_count.load(Ordering::SeqCst), 0);

        // Deliver quorum set — moves to ready, broadcast fires.
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![node_id].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };
        fetching.recv_quorum_set(qs_hash, quorum_set.clone());
        assert_eq!(broadcast_count.load(Ordering::SeqCst), 1);

        // Deliver quorum set again — envelope already moved to ready, no double-broadcast.
        fetching.cache_quorum_set(qs_hash, quorum_set);
        assert_eq!(
            broadcast_count.load(Ordering::SeqCst),
            1,
            "duplicate dep arrival must not re-trigger broadcast"
        );
    }

    /// #3845: `estimate_heap_bytes` is 0 with no buffered slots and grows once
    /// slots hold envelopes.
    #[test]
    fn test_fetching_envelopes_estimate_heap_bytes_grows() {
        let fetching = FetchingEnvelopes::with_defaults(Box::new(|_, _| false));
        assert_eq!(fetching.estimate_heap_bytes(), 0, "empty buffers cost 0");

        fetching.test_insert_ready(100, vec![make_envelope(100, 1), make_envelope(100, 2)]);
        let with_slot = fetching.estimate_heap_bytes();
        assert!(with_slot > 0, "a buffered slot must cost more than 0");

        fetching.test_insert_ready(101, vec![make_envelope(101, 3)]);
        assert!(
            fetching.estimate_heap_bytes() > with_slot,
            "a second buffered slot must increase the estimate"
        );
    }
}
