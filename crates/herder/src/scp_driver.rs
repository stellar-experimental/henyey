//! SCP driver integration for the Herder.
//!
//! This module implements the [`SCPDriver`] trait callbacks that integrate
//! the SCP consensus protocol with the Herder's transaction processing
//! and ledger management.
//!
//! # Overview
//!
//! The [`ScpDriver`] is the bridge between the SCP consensus layer and the
//! Herder's application logic. It provides:
//!
//! - **Value validation**: Checking that proposed SCP values are valid
//!   (close time is reasonable, transaction set exists, upgrades are valid)
//! - **Candidate combination**: Merging multiple candidate values into one
//! - **Envelope signing/verification**: Cryptographic operations for SCP messages
//! - **Transaction set caching**: Storing and retrieving transaction sets by hash
//! - **Externalization tracking**: Recording when slots are externalized
//! - **Quorum set management**: Storing and looking up quorum sets by node or hash
//!
//! # Key Components
//!
//! - [`ScpDriver`]: Main driver struct managing caches and cryptographic operations
//! - [`HerderScpCallback`]: Wrapper implementing the SCP callback trait
//! - [`ExternalizedSlot`]: Records a slot that has reached consensus
//! - [`PendingTxSet`]: Tracks transaction sets we need but haven't received yet

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::RwLock;
use tracing::{debug, info, trace, warn};

use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};
use henyey_common::Hash256;
use henyey_crypto::{PublicKey, SecretKey, Signature};
use henyey_ledger::LedgerManager;
use henyey_scp::{hash_quorum_set, SCPDriver, SlotIndex, ValidationLevel};
use stellar_xdr::curr::{
    EnvelopeType, LedgerUpgrade, NodeId, ReadXdr, ScpEnvelope, ScpQuorumSet, ScpStatement,
    StellarValue, StellarValueExt, Value, WriteXdr,
};

use crate::error::HerderError;
use crate::tx_queue::TransactionSet;
use crate::Result;

/// Format a `StellarValueExt` for logging.
fn describe_stellar_value_ext(ext: &StellarValueExt) -> String {
    match ext {
        StellarValueExt::Basic => "Basic".to_string(),
        StellarValueExt::Signed(sig) => {
            let node_id_bytes = match &sig.node_id.0 {
                stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key) => key.0,
            };
            format!(
                "Signed(node_id={}, sig_len={})",
                Hash256::from_bytes(node_id_bytes).to_hex(),
                sig.signature.len()
            )
        }
    }
}

/// Result of validating an SCP value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueValidation {
    /// Value is fully valid.
    Valid,
    /// Value might be valid but we're missing data.
    MaybeValid,
    /// Value is invalid.
    Invalid,
}

#[derive(Debug, Clone, Copy)]
struct TrackingState {
    index: u64,
    close_time: u64,
}

#[derive(Debug, Clone, Copy)]
struct ValueValidationContext {
    lcl_seq: u64,
    lcl_close_time: u64,
    tracking: Option<TrackingState>,
}

/// Configuration for the SCP driver.
#[derive(Debug, Clone)]
pub struct ScpDriverConfig {
    /// Our node's public key.
    pub node_id: PublicKey,
    /// Maximum transaction sets to cache.
    pub max_tx_set_cache: usize,
    /// Maximum time drift allowed (in seconds).
    pub max_time_drift: u64,
    /// Local quorum set.
    pub local_quorum_set: Option<ScpQuorumSet>,
}

impl Default for ScpDriverConfig {
    fn default() -> Self {
        Self {
            node_id: PublicKey::from_bytes(&[0u8; 32]).unwrap(),
            max_tx_set_cache: 256,
            max_time_drift: 60,
            local_quorum_set: None,
        }
    }
}

/// Cached transaction set with metadata.
#[derive(Debug, Clone)]
pub struct CachedTxSet {
    /// The transaction set.
    pub tx_set: TransactionSet,
    /// When this was cached.
    pub cached_at: std::time::Instant,
    /// Number of times this was requested.
    pub request_count: u64,
}

impl CachedTxSet {
    fn new(tx_set: TransactionSet) -> Self {
        Self {
            tx_set,
            cached_at: std::time::Instant::now(),
            request_count: 0,
        }
    }
}

/// Externalized value with metadata.
#[derive(Debug, Clone)]
pub struct ExternalizedSlot {
    /// The slot index.
    pub slot: SlotIndex,
    /// The externalized SCP value.
    pub value: Value,
    /// The transaction set hash (if resolved).
    pub tx_set_hash: Option<Hash256>,
    /// Close time from the value.
    pub close_time: u64,
    /// When this was externalized.
    pub externalized_at: std::time::Instant,
}

/// Pending transaction set request.
#[derive(Debug, Clone)]
pub struct PendingTxSet {
    /// The hash of the tx set we need.
    pub hash: Hash256,
    /// The slot this tx set is needed for.
    pub slot: SlotIndex,
    /// When we first requested this.
    pub requested_at: std::time::Instant,
    /// Number of times we've requested this.
    pub request_count: u32,
}

/// Pending quorum set request.
#[derive(Debug, Clone)]
pub struct PendingQuorumSet {
    /// Number of times we've requested this.
    pub request_count: u32,
    /// Node IDs that use this quorum set (envelope senders).
    pub node_ids: HashSet<NodeId>,
}

/// Cache sizes for diagnostics.
#[derive(Debug, Clone, Default)]
pub struct ScpDriverCacheSizes {
    /// Cached transaction sets.
    pub tx_set_cache: usize,
    /// Pending transaction set requests.
    pub pending_tx_sets: usize,
    /// Pending quorum set requests.
    pub pending_quorum_sets: usize,
    /// Externalized slots.
    pub externalized: usize,
    /// Quorum sets by node ID.
    pub quorum_sets: usize,
    /// Quorum sets by hash.
    pub quorum_sets_by_hash: usize,
}

/// Callback type for broadcasting SCP envelopes to peers.
type EnvelopeSender = Box<dyn Fn(ScpEnvelope) + Send + Sync>;

/// SCP driver that integrates consensus with the Herder.
///
/// This manages:
/// - Transaction set caching by hash
/// - Value validation callbacks
/// - Envelope signing and verification
/// - Externalized value tracking
/// - Quorum set storage and lookup
pub struct ScpDriver {
    /// Configuration.
    config: ScpDriverConfig,
    /// Secret key for signing (None if not a validator).
    secret_key: Option<SecretKey>,
    /// Cached transaction sets by hash.
    tx_set_cache: DashMap<Hash256, CachedTxSet>,
    /// Pending transaction set requests (hashes we need but don't have).
    pending_tx_sets: DashMap<Hash256, PendingTxSet>,
    /// Pending quorum set requests (hashes we need but don't have).
    pending_quorum_sets: DashMap<Hash256, PendingQuorumSet>,
    /// Externalized slots.
    externalized: RwLock<HashMap<SlotIndex, ExternalizedSlot>>,
    /// Latest externalized slot.
    latest_externalized: RwLock<Option<SlotIndex>>,
    /// Envelope broadcast callback.
    envelope_sender: RwLock<Option<EnvelopeSender>>,
    /// Network ID for signing.
    network_id: Hash256,
    /// Quorum sets by node ID (key is 32-byte public key).
    quorum_sets: DashMap<[u8; 32], ScpQuorumSet>,
    /// Quorum sets by quorum set hash.
    quorum_sets_by_hash: DashMap<Hash256, ScpQuorumSet>,
    /// Our local quorum set.
    local_quorum_set: RwLock<Option<ScpQuorumSet>>,
    /// Ledger manager for network configuration lookups.
    ledger_manager: RwLock<Option<Arc<LedgerManager>>>,
    /// Whether we are in tracking state (matching stellar-core `mHerder.isTracking()`).
    is_tracking: RwLock<bool>,
    /// Next consensus ledger index (matches stellar-core `mHerder.nextConsensusLedgerIndex()`).
    /// This is tracking_slot in Herder terms.
    tracking_consensus_index: RwLock<u64>,
    /// Tracking consensus close time (matches stellar-core `mHerder.trackingConsensusCloseTime()`).
    tracking_consensus_close_time: RwLock<u64>,
}

impl ScpDriver {
    /// Create a new SCP driver.
    pub fn new(config: ScpDriverConfig, network_id: Hash256) -> Self {
        let local_quorum_set = config.local_quorum_set.clone();
        let quorum_sets = DashMap::new();
        let quorum_sets_by_hash = DashMap::new();

        if let Some(ref quorum_set) = local_quorum_set {
            let hash = hash_quorum_set(quorum_set);
            quorum_sets_by_hash.insert(hash, quorum_set.clone());
            quorum_sets.insert(*config.node_id.as_bytes(), quorum_set.clone());
        }
        Self {
            config,
            secret_key: None,
            tx_set_cache: DashMap::new(),
            pending_tx_sets: DashMap::new(),
            pending_quorum_sets: DashMap::new(),
            externalized: RwLock::new(HashMap::new()),
            latest_externalized: RwLock::new(None),
            envelope_sender: RwLock::new(None),
            network_id,
            quorum_sets,
            quorum_sets_by_hash,
            local_quorum_set: RwLock::new(local_quorum_set),
            ledger_manager: RwLock::new(None),
            is_tracking: RwLock::new(false),
            tracking_consensus_index: RwLock::new(0),
            tracking_consensus_close_time: RwLock::new(0),
        }
    }

    /// Create a new SCP driver with a secret key for signing.
    pub fn with_secret_key(
        config: ScpDriverConfig,
        network_id: Hash256,
        secret_key: SecretKey,
    ) -> Self {
        let mut driver = Self::new(config, network_id);
        driver.secret_key = Some(secret_key);
        driver
    }

    /// Set the envelope broadcast callback.
    pub fn set_envelope_sender<F>(&self, sender: F)
    where
        F: Fn(ScpEnvelope) + Send + Sync + 'static,
    {
        *self.envelope_sender.write() = Some(Box::new(sender));
    }

    /// Provide ledger manager access for network configuration lookups.
    pub fn set_ledger_manager(&self, manager: Arc<LedgerManager>) {
        *self.ledger_manager.write() = Some(manager);
    }

    /// Update the tracking consensus state from the Herder.
    ///
    /// Called by the Herder whenever tracking state changes (bootstrap, advance_tracking_slot).
    /// This provides the ScpDriver with the state needed for stellar-core-parity close-time validation.
    pub fn set_tracking_state(
        &self,
        is_tracking: bool,
        consensus_index: u64,
        consensus_close_time: u64,
    ) {
        *self.is_tracking.write() = is_tracking;
        *self.tracking_consensus_index.write() = consensus_index;
        *self.tracking_consensus_close_time.write() = consensus_close_time;
    }

    /// Cache a transaction set.
    pub fn cache_tx_set(&self, tx_set: TransactionSet) {
        let hash = tx_set.hash;

        // Check cache size limit
        if self.tx_set_cache.len() >= self.config.max_tx_set_cache {
            // Evict oldest entry
            if let Some(oldest) = self
                .tx_set_cache
                .iter()
                .min_by_key(|e| e.cached_at)
                .map(|e| *e.key())
            {
                self.tx_set_cache.remove(&oldest);
            }
        }

        self.tx_set_cache.insert(hash, CachedTxSet::new(tx_set));
    }

    /// Get a cached transaction set by hash.
    pub fn get_tx_set(&self, hash: &Hash256) -> Option<TransactionSet> {
        self.tx_set_cache.get_mut(hash).map(|mut entry| {
            entry.request_count += 1;
            entry.tx_set.clone()
        })
    }

    /// Check if a transaction set is cached.
    pub fn has_tx_set(&self, hash: &Hash256) -> bool {
        self.tx_set_cache.contains_key(hash)
    }

    /// Register a pending tx set request.
    /// Returns true if this is a new request, false if already pending.
    pub fn request_tx_set(&self, hash: Hash256, slot: SlotIndex) -> bool {
        if self.tx_set_cache.contains_key(&hash) {
            // Already have it
            return false;
        }

        if self.pending_tx_sets.contains_key(&hash) {
            // Already requested, increment count
            if let Some(mut entry) = self.pending_tx_sets.get_mut(&hash) {
                entry.request_count += 1;
            }
            return false;
        }

        // New request
        self.pending_tx_sets.insert(
            hash,
            PendingTxSet {
                hash,
                slot,
                requested_at: std::time::Instant::now(),
                request_count: 1,
            },
        );
        debug!(%hash, slot, "Registered pending tx set request");
        true
    }

    /// Register a pending quorum set request.
    /// Returns true if this is a new request, false if already pending or known.
    /// The node_id is the envelope sender that uses this quorum set.
    pub fn request_quorum_set(&self, hash: Hash256, node_id: NodeId) -> bool {
        // If we already have this quorum set, store the association with this node_id
        // Clone the quorum set before dropping the lock to avoid deadlock
        let existing_qs = self.quorum_sets_by_hash.get(&hash).map(|qs| qs.clone());
        if let Some(qs) = existing_qs {
            trace!(%hash, node_id = ?node_id, "Associating existing quorum set with node");
            self.store_quorum_set(&node_id, qs);
            return false;
        }

        // If already pending, add this node_id to the set
        if let Some(mut entry) = self.pending_quorum_sets.get_mut(&hash) {
            entry.request_count += 1;
            entry.node_ids.insert(node_id);
            return false;
        }

        // New request - create pending entry with this node_id
        let mut node_ids = HashSet::new();
        node_ids.insert(node_id);
        self.pending_quorum_sets.insert(
            hash,
            PendingQuorumSet {
                request_count: 1,
                node_ids,
            },
        );
        info!(%hash, "Registered pending quorum set request");
        true
    }

    /// Clear a quorum set request once it has been satisfied.
    pub fn clear_quorum_set_request(&self, hash: &Hash256) {
        self.pending_quorum_sets.remove(hash);
    }

    /// Get the node IDs that are waiting for a quorum set with the given hash.
    pub fn get_pending_quorum_set_node_ids(&self, hash: &Hash256) -> Vec<NodeId> {
        self.pending_quorum_sets
            .get(hash)
            .map(|entry| entry.node_ids.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get all pending tx set hashes that need to be fetched.
    pub fn get_pending_tx_set_hashes(&self) -> Vec<Hash256> {
        self.pending_tx_sets
            .iter()
            .map(|entry| *entry.key())
            .collect()
    }

    /// Get all pending tx sets with their slots.
    pub fn get_pending_tx_sets(&self) -> Vec<(Hash256, SlotIndex)> {
        self.pending_tx_sets
            .iter()
            .map(|entry| {
                let pending = entry.value();
                (pending.hash, pending.slot)
            })
            .collect()
    }

    /// Clear all pending tx set requests.
    /// Used after rapid close cycles to discard stale requests whose tx_sets
    /// are no longer available from peers.
    pub fn clear_pending_tx_sets(&self) {
        let count = self.pending_tx_sets.len();
        self.pending_tx_sets.clear();
        if count > 0 {
            debug!(cleared = count, "Cleared stale pending tx_set requests");
        }
    }

    /// Check if we need a tx set.
    pub fn needs_tx_set(&self, hash: &Hash256) -> bool {
        self.pending_tx_sets.contains_key(hash) && !self.tx_set_cache.contains_key(hash)
    }

    /// Receive a tx set from the network.
    /// Returns the slot it was needed for, if any.
    pub fn receive_tx_set(&self, tx_set: TransactionSet) -> Option<SlotIndex> {
        let hash = tx_set.hash;
        if let Some(recomputed) = tx_set.recompute_hash() {
            if recomputed != hash {
                warn!(
                    expected = %hash,
                    computed = %recomputed,
                    "Rejecting tx set with mismatched hash"
                );
                return None;
            }
        } else {
            warn!(%hash, "Rejecting tx set without recomputable hash");
            return None;
        }

        // Remove from pending
        let pending = self.pending_tx_sets.remove(&hash);
        let slot = pending.map(|(_, p)| p.slot);

        // Cache it
        self.cache_tx_set(tx_set);

        if let Some(s) = slot {
            debug!(%hash, slot = s, "Received pending tx set");
        } else {
            debug!(%hash, "Received tx set (not pending)");
        }

        slot
    }

    /// Clean up old pending requests.
    pub fn cleanup_pending_tx_sets(&self, max_age_secs: u64) {
        let cutoff = std::time::Instant::now() - std::time::Duration::from_secs(max_age_secs);
        self.pending_tx_sets.retain(|_, v| v.requested_at > cutoff);
    }

    /// Clean up pending requests for slots older than the given slot.
    /// Returns the number of requests removed.
    pub fn cleanup_old_pending_slots(&self, current_slot: SlotIndex) -> usize {
        let old_count = self.pending_tx_sets.len();
        // Only keep requests for the current slot - peers don't cache old tx sets long
        // We use >= current_slot to keep current and future (shouldn't happen but be safe)
        self.pending_tx_sets.retain(|_, v| v.slot >= current_slot);
        let new_count = self.pending_tx_sets.len();
        old_count - new_count
    }

    /// Check if any pending tx set request has been waiting longer than the given duration.
    /// Returns true if at least one request has exceeded the timeout.
    pub fn has_stale_pending_tx_set(&self, max_wait_secs: u64) -> bool {
        let now = std::time::Instant::now();
        let max_wait = std::time::Duration::from_secs(max_wait_secs);
        self.pending_tx_sets
            .iter()
            .any(|entry| now.duration_since(entry.value().requested_at) >= max_wait)
    }

    /// Get the network ID.
    pub fn network_id(&self) -> Hash256 {
        self.network_id
    }

    /// Get the latest externalized slot.
    pub fn latest_externalized_slot(&self) -> Option<SlotIndex> {
        *self.latest_externalized.read()
    }

    /// Get an externalized slot.
    pub fn get_externalized(&self, slot: SlotIndex) -> Option<ExternalizedSlot> {
        self.externalized.read().get(&slot).cloned()
    }

    /// Find the slot for a given tx set hash in recent externalized values.
    pub fn find_externalized_slot_by_tx_set_hash(&self, hash: &Hash256) -> Option<SlotIndex> {
        self.externalized.read().iter().find_map(|(slot, ext)| {
            ext.tx_set_hash
                .as_ref()
                .filter(|tx_hash| *tx_hash == hash)
                .map(|_| *slot)
        })
    }

    /// Get all externalized slot indices in a range (inclusive).
    /// Returns a sorted list of slots that have been externalized.
    pub fn get_externalized_slots_in_range(
        &self,
        from: SlotIndex,
        to: SlotIndex,
    ) -> Vec<SlotIndex> {
        let externalized = self.externalized.read();
        let mut slots: Vec<SlotIndex> = externalized
            .keys()
            .filter(|&&slot| slot >= from && slot <= to)
            .copied()
            .collect();
        slots.sort();
        slots
    }

    /// Find missing (gap) slots in a range that have not been externalized.
    /// Returns slots that should have EXTERNALIZE but don't.
    pub fn find_missing_slots_in_range(&self, from: SlotIndex, to: SlotIndex) -> Vec<SlotIndex> {
        if from > to {
            return vec![];
        }
        let externalized = self.externalized.read();
        let mut missing = Vec::new();
        for slot in from..=to {
            if !externalized.contains_key(&slot) {
                missing.push(slot);
            }
        }
        missing
    }

    /// Validate close time against a last close time reference.
    ///
    /// Matches stellar-core `HerderSCPDriver::checkCloseTime(slotIndex, lastCloseTime, sv)`.
    /// Returns true if:
    /// 1. close_time > lastCloseTime (not too old)
    /// 2. close_time <= now + MAX_TIME_SLIP_SECONDS (not too far in future)
    pub fn check_close_time(
        &self,
        _slot_index: SlotIndex,
        last_close_time: u64,
        close_time: u64,
    ) -> bool {
        // Check closeTime (not too old)
        if close_time <= last_close_time {
            trace!(
                "Close time {} not after last close time {}",
                close_time,
                last_close_time
            );
            return false;
        }

        // Check closeTime (not too far in future)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before UNIX epoch")
            .as_secs();
        if close_time > now + self.config.max_time_drift {
            trace!(
                "Close time {} too far in future (now: {}, max_slip: {})",
                close_time,
                now,
                self.config.max_time_drift
            );
            return false;
        }
        true
    }

    /// Validate a value for a past or future slot (not LCL+1).
    ///
    /// Matches stellar-core `HerderSCPDriver::validatePastOrFutureValue`.
    ///
    /// # Arguments
    /// * `slot_index` - The slot being validated
    /// * `close_time` - The close time of the value
    /// * `lcl_seq` - The LCL's ledger sequence
    /// * `lcl_close_time` - The LCL's close time
    /// * `is_tracking` - Whether we are in tracking state
    /// * `tracking_index` - Next consensus ledger index (tracking_slot)
    /// * `tracking_close_time` - Tracking consensus close time
    fn validate_past_or_future_value(
        &self,
        slot_index: SlotIndex,
        close_time: u64,
        context: ValueValidationContext,
    ) -> ValueValidation {
        let ValueValidationContext {
            lcl_seq,
            lcl_close_time,
            tracking,
        } = context;

        // slot_index must NOT be lcl_seq + 1 (that's the current ledger path)
        if slot_index == lcl_seq + 1 {
            debug!(
                "validate_past_or_future_value called for current ledger {}",
                slot_index
            );
            return ValueValidation::Invalid;
        }

        if slot_index == lcl_seq {
            // Previous ledger: close time must exactly match LCL
            if close_time != lcl_close_time {
                trace!(
                    "Bad close time for ledger {}: got {} vs LCL {}",
                    slot_index,
                    close_time,
                    lcl_close_time
                );
                return ValueValidation::Invalid;
            }
        } else if slot_index < lcl_seq {
            // Older than LCL: close time must be strictly less
            if close_time >= lcl_close_time {
                trace!(
                    "Bad close time for old ledger {}: got {} vs LCL {}",
                    slot_index,
                    close_time,
                    lcl_close_time
                );
                return ValueValidation::Invalid;
            }
        } else {
            // Future slot (beyond LCL+1): use checkCloseTime with LCL as reference
            if !self.check_close_time(slot_index, lcl_close_time, close_time) {
                return ValueValidation::Invalid;
            }
        }

        let Some(tracking) = tracking else {
            // Can't validate further without tracking state
            trace!("MaybeValidValue (not tracking) for slot {}", slot_index);
            return ValueValidation::MaybeValid;
        };

        let TrackingState {
            index: tracking_index,
            close_time: tracking_close_time,
        } = tracking;

        // Check slotIndex against tracking state
        if tracking_index > slot_index {
            // We already moved on from this slot
            trace!(
                "MaybeValidValue (already moved on) for slot {}, at {}",
                slot_index,
                tracking_index
            );
            return ValueValidation::MaybeValid;
        }
        if tracking_index < slot_index {
            // Processing a future message while tracking -- should not happen
            debug!(
                "validateValue slot {} processing future message while tracking {}",
                slot_index,
                tracking_index.saturating_sub(1)
            );
            return ValueValidation::Invalid;
        }

        // tracking_index == slot_index: use tracking close time for tighter check
        if !self.check_close_time(slot_index, tracking_close_time, close_time) {
            return ValueValidation::Invalid;
        }

        trace!(
            "Can't validate locally, value may be valid for slot {}",
            slot_index
        );
        ValueValidation::MaybeValid
    }

    /// Validate an SCP value.
    ///
    /// The value is the XDR-encoded StellarValue.
    /// Matches stellar-core `HerderSCPDriver::validateValue` which:
    /// 1. Deserializes to StellarValue
    /// 2. Checks STELLAR_VALUE_SIGNED (required for ALL values)
    /// 3. Verifies the signature
    /// 4. Validates close time and tx set (via validateValueAgainstLocalState)
    /// 5. Checks upgrade ordering
    pub fn validate_value_impl(&self, slot_index: SlotIndex, value: &Value) -> ValueValidation {
        // Decode the StellarValue
        let stellar_value = match StellarValue::from_xdr(value, stellar_xdr::curr::Limits::none()) {
            Ok(v) => v,
            Err(e) => {
                debug!("Failed to decode StellarValue: {}", e);
                return ValueValidation::Invalid;
            }
        };

        // Parity: check STELLAR_VALUE_SIGNED (required for both nomination and ballot)
        let sig = match &stellar_value.ext {
            StellarValueExt::Signed(sig) => sig,
            StellarValueExt::Basic => {
                debug!("Expected STELLAR_VALUE_SIGNED");
                return ValueValidation::Invalid;
            }
        };

        // Parity: verify the stellar value signature
        // Signs: (networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)
        if !self.verify_stellar_value_signature(
            &sig.node_id,
            &sig.signature,
            &stellar_value.tx_set_hash,
            stellar_value.close_time.clone(),
        ) {
            debug!("StellarValue signature verification failed");
            return ValueValidation::Invalid;
        }

        // Validate against local state (close time, tx set, etc.)
        let result = self.validate_value_against_local_state(slot_index, &stellar_value);
        if result == ValueValidation::Invalid {
            return ValueValidation::Invalid;
        }

        // Check upgrade ordering and validity (regardless of local state result)
        if !Self::check_upgrade_ordering(&stellar_value) {
            return ValueValidation::Invalid;
        }

        // Parity: HerderSCPDriver.cpp:375-401 — validate each upgrade via isValid
        if !self.check_upgrades_valid(&stellar_value) {
            return ValueValidation::Invalid;
        }

        result
    }

    /// Validate a StellarValue against local state.
    ///
    /// Checks close time and transaction set validity.
    /// Matches stellar-core `HerderSCPDriver::validateValueAgainstLocalState`.
    ///
    /// For LCL+1 (current ledger): performs full validation (close time + tx set).
    /// For past/future slots: delegates to `validate_past_or_future_value`.
    fn validate_value_against_local_state(
        &self,
        slot_index: SlotIndex,
        stellar_value: &StellarValue,
    ) -> ValueValidation {
        let close_time = stellar_value.close_time.0;

        // Get LCL data from ledger manager
        let (lcl_seq, lcl_close_time) = if let Some(ref lm) = *self.ledger_manager.read() {
            let header = lm.current_header();
            (header.ledger_seq as u64, header.scp_value.close_time.0)
        } else {
            // No ledger manager available — fall back to externalized data
            if let Some(latest) = *self.latest_externalized.read() {
                if let Some(externalized) = self.externalized.read().get(&latest) {
                    (externalized.slot, externalized.close_time)
                } else {
                    (0, 0)
                }
            } else if *self.is_tracking.read() {
                // When tracking but no ledger manager or externalized data (e.g., after
                // bootstrap), derive the LCL from the tracking consensus index.
                // tracking_consensus_index is the next slot to close, so LCL = index - 1.
                let tracking_index = *self.tracking_consensus_index.read();
                let tracking_close_time = *self.tracking_consensus_close_time.read();
                (tracking_index.saturating_sub(1), tracking_close_time)
            } else {
                (0, 0)
            }
        };

        let is_current_ledger = slot_index == lcl_seq + 1;

        if is_current_ledger {
            // The value is for LCL+1 — perform all possible checks
            if !self.check_close_time(slot_index, lcl_close_time, close_time) {
                return ValueValidation::Invalid;
            }

            // Check if we have the transaction set
            let tx_set_hash = Hash256::from_bytes(stellar_value.tx_set_hash.0);
            if !self.has_tx_set(&tx_set_hash) {
                debug!("Missing transaction set: {}", tx_set_hash);
                return ValueValidation::Invalid;
            }
            if let Some(tx_set) = self.tx_set_cache.get(&tx_set_hash) {
                // Parity: verify hash integrity
                match tx_set.tx_set.recompute_hash() {
                    Some(computed) if computed == tx_set_hash => {}
                    Some(computed) => {
                        debug!(
                            "Tx set hash mismatch: expected {}, computed {}",
                            tx_set_hash, computed
                        );
                        return ValueValidation::Invalid;
                    }
                    None => {
                        debug!("Failed to recompute tx set hash");
                        return ValueValidation::Invalid;
                    }
                }

                // Parity: check previousLedgerHash matches the LCL hash
                if let Some(ref lm) = *self.ledger_manager.read() {
                    let lcl_hash = lm.current_header_hash();
                    if tx_set.tx_set.previous_ledger_hash != lcl_hash {
                        debug!(
                            "Tx set previousLedgerHash mismatch: expected {}, got {}",
                            lcl_hash, tx_set.tx_set.previous_ledger_hash
                        );
                        return ValueValidation::Invalid;
                    }
                }

                // Parity: validate tx set is well-formed (sorted, no duplicates)
                if !Self::is_tx_set_well_formed(&tx_set.tx_set) {
                    debug!("Tx set is not well-formed (unsorted or has duplicates)");
                    return ValueValidation::Invalid;
                }
            }

            ValueValidation::Valid
        } else {
            // Past or future slot — partial validation
            let tracking = if *self.is_tracking.read() {
                Some(TrackingState {
                    index: *self.tracking_consensus_index.read(),
                    close_time: *self.tracking_consensus_close_time.read(),
                })
            } else {
                None
            };

            self.validate_past_or_future_value(
                slot_index,
                close_time,
                ValueValidationContext {
                    lcl_seq,
                    lcl_close_time,
                    tracking,
                },
            )
        }
    }

    /// Check that upgrades in a StellarValue are in strictly increasing order.
    fn check_upgrade_ordering(stellar_value: &StellarValue) -> bool {
        let mut last_upgrade_order = None;
        for upgrade in stellar_value.upgrades.iter() {
            let upgrade = match LedgerUpgrade::from_xdr(
                upgrade.0.as_slice(),
                stellar_xdr::curr::Limits::none(),
            ) {
                Ok(upgrade) => upgrade,
                Err(_) => {
                    debug!("Invalid ledger upgrade encountered");
                    return false;
                }
            };
            let order = Self::upgrade_type_order(&upgrade);
            if last_upgrade_order.is_some_and(|prev| order <= prev) {
                debug!("Invalid ledger upgrade encountered");
                return false;
            }
            last_upgrade_order = Some(order);
        }
        true
    }

    /// Verify a StellarValue signature.
    ///
    /// stellar-core signs: `(networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)`.
    fn verify_stellar_value_signature(
        &self,
        node_id: &NodeId,
        signature: &stellar_xdr::curr::Signature,
        tx_set_hash: &stellar_xdr::curr::Hash,
        close_time: stellar_xdr::curr::TimePoint,
    ) -> bool {
        let public_key = match PublicKey::try_from(&node_id.0) {
            Ok(pk) => pk,
            Err(_) => return false,
        };

        // Build signed data: (networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)
        let mut data = self.network_id.0.to_vec();
        if let Ok(env_type_bytes) = EnvelopeType::Scpvalue.to_xdr(stellar_xdr::curr::Limits::none())
        {
            data.extend_from_slice(&env_type_bytes);
        } else {
            return false;
        }
        if let Ok(hash_bytes) = tx_set_hash.to_xdr(stellar_xdr::curr::Limits::none()) {
            data.extend_from_slice(&hash_bytes);
        } else {
            return false;
        }
        if let Ok(time_bytes) = close_time.to_xdr(stellar_xdr::curr::Limits::none()) {
            data.extend_from_slice(&time_bytes);
        } else {
            return false;
        }

        let sig_bytes: [u8; 64] = match signature.0.as_slice().try_into() {
            Ok(b) => b,
            Err(_) => return false,
        };

        let sig = Signature::from_bytes(sig_bytes);
        public_key.verify(&data, &sig).is_ok()
    }

    /// Extract a valid value from a potentially invalid composite.
    ///
    /// Parity: `HerderSCPDriver::extractValidValue`:
    /// 1. Does NOT check STELLAR_VALUE_SIGNED or verify signature
    /// 2. Calls validateValueAgainstLocalState with nomination=true
    /// 3. Only returns a value when result is kFullyValidatedValue
    /// 4. Strips invalid upgrades from the value
    pub fn extract_valid_value_impl(&self, slot: SlotIndex, value: &Value) -> Option<Value> {
        if value.0.is_empty() {
            return None;
        }

        // Decode the StellarValue
        let mut stellar_value =
            match StellarValue::from_xdr(value, stellar_xdr::curr::Limits::none()) {
                Ok(v) => v,
                Err(_) => return None,
            };

        // Parity: only extract if fully validated against local state
        // (does NOT check STELLAR_VALUE_SIGNED or signature)
        let result = self.validate_value_against_local_state(slot, &stellar_value);
        if result != ValueValidation::Valid {
            return None;
        }

        // Parity: strip invalid upgrades, keeping valid ones in order
        // Also validates each upgrade via isValidForApply
        let current_version = self
            .ledger_manager
            .read()
            .as_ref()
            .map(|lm| lm.current_header().ledger_version)
            .unwrap_or(0);
        let mut valid_upgrades = Vec::new();
        let mut last_upgrade_type = None;
        for upgrade_bytes in stellar_value.upgrades.iter() {
            if let Ok(upgrade) = LedgerUpgrade::from_xdr(
                upgrade_bytes.0.as_slice(),
                stellar_xdr::curr::Limits::none(),
            ) {
                let upgrade_type = Self::upgrade_type_order(&upgrade);
                // Only keep if in strictly increasing order and valid for apply
                let in_order = last_upgrade_type
                    .map(|prev| upgrade_type > prev)
                    .unwrap_or(true);
                if in_order && Self::is_valid_upgrade_for_apply(&upgrade, current_version) {
                    last_upgrade_type = Some(upgrade_type);
                    valid_upgrades.push(upgrade_bytes.clone());
                }
            }
        }

        // If upgrades changed, update the value
        if valid_upgrades.len() != stellar_value.upgrades.len() {
            stellar_value.upgrades = valid_upgrades.try_into().unwrap_or_default();
            // Re-encode
            stellar_value
                .to_xdr(stellar_xdr::curr::Limits::none())
                .ok()
                .map(|bytes| Value(bytes.try_into().unwrap_or_default()))
        } else {
            Some(value.clone())
        }
    }

    /// Check that all upgrades in a StellarValue are valid for application.
    ///
    /// Parity: Upgrades.cpp `isValid` → `isValidForApply`
    fn check_upgrades_valid(&self, stellar_value: &StellarValue) -> bool {
        let current_version = if let Some(ref lm) = *self.ledger_manager.read() {
            lm.current_header().ledger_version
        } else {
            return true; // No ledger manager — can't validate
        };

        for upgrade_bytes in stellar_value.upgrades.iter() {
            let upgrade = match LedgerUpgrade::from_xdr(
                upgrade_bytes.0.as_slice(),
                stellar_xdr::curr::Limits::none(),
            ) {
                Ok(u) => u,
                Err(_) => return false,
            };
            if !Self::is_valid_upgrade_for_apply(&upgrade, current_version) {
                debug!(?upgrade, "Invalid upgrade for apply");
                return false;
            }
        }
        true
    }

    /// Check if a single upgrade is valid for application to the ledger.
    ///
    /// Parity: Upgrades.cpp `isValidForApply` (lines 543-616)
    fn is_valid_upgrade_for_apply(upgrade: &LedgerUpgrade, current_version: u32) -> bool {
        match upgrade {
            LedgerUpgrade::Version(new_version) => {
                // Must be strictly monotonic and within supported range
                *new_version > current_version
                    && *new_version <= henyey_common::CURRENT_LEDGER_PROTOCOL_VERSION
            }
            LedgerUpgrade::BaseFee(fee) => *fee != 0,
            LedgerUpgrade::MaxTxSetSize(_) => true, // Any size allowed
            LedgerUpgrade::BaseReserve(reserve) => *reserve != 0,
            LedgerUpgrade::Flags(flags) => {
                // Must be protocol >= 18 and only valid flag bits
                const MASK_LEDGER_HEADER_FLAGS: u32 = 0x7;
                protocol_version_starts_from(current_version, ProtocolVersion::V18)
                    && (*flags & !MASK_LEDGER_HEADER_FLAGS) == 0
            }
            LedgerUpgrade::Config(_) => {
                // Config upgrades require Soroban protocol.
                // Full validation would load the config set from ledger,
                // but basic version check is the critical gate.
                current_version >= henyey_common::MIN_SOROBAN_PROTOCOL_VERSION
            }
            LedgerUpgrade::MaxSorobanTxSetSize(_) => {
                current_version >= henyey_common::MIN_SOROBAN_PROTOCOL_VERSION
            }
        }
    }

    /// Get the ordering number for an upgrade type.
    fn upgrade_type_order(upgrade: &LedgerUpgrade) -> u32 {
        match upgrade {
            LedgerUpgrade::Version(_) => 0,
            LedgerUpgrade::BaseFee(_) => 1,
            LedgerUpgrade::MaxTxSetSize(_) => 2,
            LedgerUpgrade::BaseReserve(_) => 3,
            LedgerUpgrade::Flags(_) => 4,
            LedgerUpgrade::Config(_) => 5,
            LedgerUpgrade::MaxSorobanTxSetSize(_) => 6,
        }
    }

    /// Combine multiple candidate values into one.
    ///
    /// Parity: `HerderSCPDriver::combineCandidates`:
    /// 1. Collect upgrades from ALL candidates, merging by taking max of each type
    /// 2. Select the best tx set using compareTxSets (size comparison + tiebreak)
    /// 3. Compose result: best candidate's txSetHash/closeTime + merged upgrades
    pub fn combine_candidates_impl(&self, _slot: SlotIndex, values: &[Value]) -> Value {
        if values.is_empty() {
            return Value::default();
        }

        if values.len() == 1 {
            return values[0].clone();
        }

        // Decode all values
        let mut decoded: Vec<StellarValue> = values
            .iter()
            .filter_map(|v| StellarValue::from_xdr(v, stellar_xdr::curr::Limits::none()).ok())
            .collect();

        if decoded.is_empty() {
            return values[0].clone();
        }

        // Parity: filter out candidates whose tx set has a different previousLedgerHash
        if let Some(ref lm) = *self.ledger_manager.read() {
            let lcl_hash = lm.current_header_hash();
            decoded.retain(|sv| {
                let tx_set_hash = Hash256::from_bytes(sv.tx_set_hash.0);
                if let Some(tx_set) = self.tx_set_cache.get(&tx_set_hash) {
                    tx_set.tx_set.previous_ledger_hash == lcl_hash
                } else {
                    // Can't verify — keep it
                    true
                }
            });
            if decoded.is_empty() {
                // All candidates filtered out — fall back to first value
                return values[0].clone();
            }
        }

        // Step 1: Compute candidates hash (XOR of all candidate hashes) for tiebreaking
        let mut candidates_hash = [0u8; 32];
        for sv in &decoded {
            let val_bytes = sv
                .to_xdr(stellar_xdr::curr::Limits::none())
                .unwrap_or_default();
            let hash = Hash256::hash(&val_bytes);
            for (i, byte) in candidates_hash.iter_mut().enumerate() {
                *byte ^= hash.as_bytes()[i];
            }
        }

        // Step 2: Merge upgrades across all candidates (take max of each upgrade type)
        let mut merged_upgrades: std::collections::BTreeMap<u32, LedgerUpgrade> =
            std::collections::BTreeMap::new();
        for sv in &decoded {
            for upgrade_bytes in sv.upgrades.iter() {
                if let Ok(upgrade) = LedgerUpgrade::from_xdr(
                    upgrade_bytes.0.as_slice(),
                    stellar_xdr::curr::Limits::none(),
                ) {
                    let order = Self::upgrade_type_order(&upgrade);
                    merged_upgrades
                        .entry(order)
                        .and_modify(|existing| {
                            if Self::compare_upgrades(&upgrade, existing) {
                                *existing = upgrade.clone();
                            }
                        })
                        .or_insert(upgrade);
                }
            }
        }

        // Step 3: Select best candidate using compareTxSets logic
        // Parity: HerderSCPDriver.cpp:614-653 compareTxSets
        // 1. More operations wins
        // 2. Higher total fees wins
        // 3. XOR hash tiebreak with candidates_hash
        let best_idx = decoded
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                let a_hash = Hash256::from_bytes(a.tx_set_hash.0);
                let b_hash = Hash256::from_bytes(b.tx_set_hash.0);
                self.compare_tx_sets(&a_hash, &b_hash, &candidates_hash)
            })
            .map(|(i, _)| i)
            .unwrap_or(0);

        // Step 4: Compose result
        let mut result = decoded[best_idx].clone();

        // Replace upgrades with merged set (in order of upgrade type)
        let upgrade_bytes: Vec<stellar_xdr::curr::UpgradeType> = merged_upgrades
            .values()
            .filter_map(|upgrade| {
                upgrade
                    .to_xdr(stellar_xdr::curr::Limits::none())
                    .ok()
                    .and_then(|bytes| stellar_xdr::curr::UpgradeType(bytes.try_into().ok()?).into())
            })
            .collect();
        result.upgrades = upgrade_bytes.try_into().unwrap_or_default();

        // Re-encode the result
        result
            .to_xdr(stellar_xdr::curr::Limits::none())
            .map(|bytes| Value(bytes.try_into().unwrap_or_default()))
            .unwrap_or_default()
    }

    /// Compare two upgrades of the same type, returning true if `new` > `existing`.
    /// stellar-core takes the max of each upgrade type.
    fn compare_upgrades(new: &LedgerUpgrade, existing: &LedgerUpgrade) -> bool {
        match (new, existing) {
            (LedgerUpgrade::Version(a), LedgerUpgrade::Version(b)) => a > b,
            (LedgerUpgrade::BaseFee(a), LedgerUpgrade::BaseFee(b)) => a > b,
            (LedgerUpgrade::MaxTxSetSize(a), LedgerUpgrade::MaxTxSetSize(b)) => a > b,
            (LedgerUpgrade::BaseReserve(a), LedgerUpgrade::BaseReserve(b)) => a > b,
            (LedgerUpgrade::Flags(a), LedgerUpgrade::Flags(b)) => a > b,
            (LedgerUpgrade::Config(a), LedgerUpgrade::Config(b)) => {
                // ConfigUpgradeSetKey derives Ord: compares contractID then contentHash
                a > b
            }
            (LedgerUpgrade::MaxSorobanTxSetSize(a), LedgerUpgrade::MaxSorobanTxSetSize(b)) => a > b,
            _ => false, // Different types shouldn't happen
        }
    }

    /// Compare two transaction sets for combine_candidates.
    ///
    /// Spec: HERDER_SPEC §10.2 — 5-criteria ordered comparison:
    /// 1. Most operations (more is better)
    /// 2. Highest total inclusion fees (more is better)
    /// 3. Highest total full fees (more is better)
    /// 4. Smallest encoded size (less is better)
    /// 5. XOR hash tiebreak (lexicographically smallest)
    fn compare_tx_sets(
        &self,
        a_hash: &Hash256,
        b_hash: &Hash256,
        candidates_hash: &[u8; 32],
    ) -> std::cmp::Ordering {
        let a_set = self.tx_set_cache.get(a_hash);
        let b_set = self.tx_set_cache.get(b_hash);

        if let (Some(a), Some(b)) = (a_set.as_ref(), b_set.as_ref()) {
            // 1. Compare by number of operations (more is better)
            let a_ops = Self::tx_set_num_ops(&a.tx_set);
            let b_ops = Self::tx_set_num_ops(&b.tx_set);
            let ops_cmp = a_ops.cmp(&b_ops);
            if ops_cmp != std::cmp::Ordering::Equal {
                return ops_cmp;
            }

            // 2. Compare by total inclusion fees (higher is better)
            let a_inclusion_fees = Self::tx_set_total_inclusion_fees(&a.tx_set);
            let b_inclusion_fees = Self::tx_set_total_inclusion_fees(&b.tx_set);
            let inclusion_fees_cmp = a_inclusion_fees.cmp(&b_inclusion_fees);
            if inclusion_fees_cmp != std::cmp::Ordering::Equal {
                return inclusion_fees_cmp;
            }

            // 3. Compare by total full fees (higher is better)
            let a_fees = Self::tx_set_total_fees(&a.tx_set);
            let b_fees = Self::tx_set_total_fees(&b.tx_set);
            let fees_cmp = a_fees.cmp(&b_fees);
            if fees_cmp != std::cmp::Ordering::Equal {
                return fees_cmp;
            }

            // 4. Compare by encoded size (smaller is better — note reversed order)
            let a_size = Self::tx_set_encoded_size(&a.tx_set);
            let b_size = Self::tx_set_encoded_size(&b.tx_set);
            let size_cmp = b_size.cmp(&a_size); // reversed: smaller is better
            if size_cmp != std::cmp::Ordering::Equal {
                return size_cmp;
            }
        }

        // 5. XOR hash tiebreak
        let a_xored = Self::xor_hash(&a_hash.0, candidates_hash);
        let b_xored = Self::xor_hash(&b_hash.0, candidates_hash);
        a_xored.cmp(&b_xored)
    }

    /// Check that a transaction set is well-formed: sorted by hash and no duplicates.
    ///
    /// Parity: stellar-core `TxSetUtils::checkValid()` verifies structural integrity.
    fn is_tx_set_well_formed(tx_set: &TransactionSet) -> bool {
        let txs = &tx_set.transactions;
        if txs.len() <= 1 {
            return true;
        }

        let mut prev_hash = Hash256::hash_xdr(&txs[0]).unwrap_or(Hash256::ZERO);
        for tx in &txs[1..] {
            let hash = Hash256::hash_xdr(tx).unwrap_or(Hash256::ZERO);
            if hash.0 <= prev_hash.0 {
                // Not strictly ascending — either unsorted or duplicate
                return false;
            }
            prev_hash = hash;
        }

        true
    }

    /// Count total number of operations in a transaction set.
    fn tx_set_num_ops(tx_set: &TransactionSet) -> usize {
        tx_set
            .transactions
            .iter()
            .map(|env| Self::envelope_num_ops(env))
            .sum()
    }

    /// Compute total inclusion fees for a transaction set.
    ///
    /// For generalized tx sets, the inclusion fee per tx is
    /// `min(tx.fee, numOps * componentBaseFee)`. For legacy tx sets,
    /// inclusion fee == full fee (no discounting).
    fn tx_set_total_inclusion_fees(tx_set: &TransactionSet) -> i64 {
        let Some(ref gen) = tx_set.generalized_tx_set else {
            // Legacy tx set: inclusion fee == full fee
            return Self::tx_set_total_fees(tx_set);
        };
        let stellar_xdr::curr::GeneralizedTransactionSet::V1(set_v1) = gen;

        let mut total = 0i64;
        for phase in set_v1.phases.iter() {
            for (tx, base_fee) in Self::phase_txs_with_base_fee(phase) {
                total += crate::tx_set_utils::inclusion_fee(tx, base_fee);
            }
        }
        total
    }

    /// Iterate over all transactions in a phase, yielding each envelope
    /// together with the component's optional base fee.
    fn phase_txs_with_base_fee(
        phase: &stellar_xdr::curr::TransactionPhase,
    ) -> Vec<(&stellar_xdr::curr::TransactionEnvelope, Option<i64>)> {
        match phase {
            stellar_xdr::curr::TransactionPhase::V0(components) => components
                .iter()
                .flat_map(|comp| {
                    let stellar_xdr::curr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(c) = comp;
                    c.txs.iter().map(move |tx| (tx, c.base_fee))
                })
                .collect(),
            stellar_xdr::curr::TransactionPhase::V1(parallel) => parallel
                .execution_stages
                .iter()
                .flat_map(|stage| stage.iter())
                .flat_map(|cluster| cluster.0.iter())
                .map(|tx| (tx, parallel.base_fee))
                .collect(),
        }
    }

    /// Compute total fees for a transaction set.
    fn tx_set_total_fees(tx_set: &TransactionSet) -> i64 {
        tx_set
            .transactions
            .iter()
            .map(|env| Self::envelope_fee(env))
            .sum()
    }

    /// Get fee from a transaction envelope.
    fn envelope_fee(env: &stellar_xdr::curr::TransactionEnvelope) -> i64 {
        crate::tx_set_utils::envelope_fee(env)
    }

    /// Get number of operations from a transaction envelope.
    fn envelope_num_ops(env: &stellar_xdr::curr::TransactionEnvelope) -> usize {
        crate::tx_set_utils::envelope_num_ops(env)
    }

    /// Compute XDR-encoded size of a transaction set.
    ///
    /// For generalized tx sets, encodes the generalized set. For legacy,
    /// sums the XDR-encoded size of all transactions.
    fn tx_set_encoded_size(tx_set: &TransactionSet) -> usize {
        if let Some(ref gen) = tx_set.generalized_tx_set {
            gen.to_xdr(stellar_xdr::curr::Limits::none())
                .map(|bytes| bytes.len())
                .unwrap_or(0)
        } else {
            tx_set
                .transactions
                .iter()
                .map(|tx| {
                    tx.to_xdr(stellar_xdr::curr::Limits::none())
                        .map(|bytes| bytes.len())
                        .unwrap_or(0)
                })
                .sum()
        }
    }

    /// XOR a 32-byte hash with another 32-byte value for tiebreaking.
    fn xor_hash(hash: &[u8; 32], mask: &[u8; 32]) -> [u8; 32] {
        let mut result = [0u8; 32];
        for i in 0..32 {
            result[i] = hash[i] ^ mask[i];
        }
        result
    }

    /// Sign an SCP envelope.
    pub fn sign_envelope(&self, statement: &ScpStatement) -> Option<Signature> {
        let secret_key = self.secret_key.as_ref()?;

        // Create the data to sign: network ID + ENVELOPE_TYPE_SCP + statement XDR
        // ENVELOPE_TYPE_SCP = 1 (as i32 big-endian)
        let statement_bytes = statement.to_xdr(stellar_xdr::curr::Limits::none()).ok()?;
        let mut data = self.network_id.0.to_vec();
        data.extend_from_slice(&1i32.to_be_bytes()); // ENVELOPE_TYPE_SCP
        data.extend_from_slice(&statement_bytes);

        Some(secret_key.sign(&data))
    }

    /// Verify an SCP envelope signature.
    pub fn verify_envelope(&self, envelope: &ScpEnvelope) -> Result<()> {
        // Extract the public key from the node ID
        let node_id = &envelope.statement.node_id;
        let public_key = PublicKey::try_from(&node_id.0)
            .map_err(|_| HerderError::Internal("Invalid node ID".to_string()))?;

        // Create the data that was signed: network ID + ENVELOPE_TYPE_SCP + statement XDR
        // ENVELOPE_TYPE_SCP = 1 (as i32 big-endian)
        let statement_bytes = envelope
            .statement
            .to_xdr(stellar_xdr::curr::Limits::none())
            .map_err(|e| HerderError::Internal(format!("Failed to encode statement: {}", e)))?;

        let mut data = self.network_id.0.to_vec();
        data.extend_from_slice(&1i32.to_be_bytes()); // ENVELOPE_TYPE_SCP
        data.extend_from_slice(&statement_bytes);

        // Verify signature
        let sig_bytes: [u8; 64] = envelope
            .signature
            .0
            .as_slice()
            .try_into()
            .map_err(|_| HerderError::Internal("Invalid signature length".to_string()))?;

        let signature = henyey_crypto::Signature::from_bytes(sig_bytes);

        public_key
            .verify(&data, &signature)
            .map_err(|_| HerderError::Scp(henyey_scp::ScpError::SignatureVerificationFailed))
    }

    /// Construct and broadcast an EXTERNALIZE envelope for a slot that was
    /// fast-forwarded (not processed through full SCP ballot protocol).
    ///
    /// This is needed so that network crawlers (e.g. StellarBeat/OBSRVR) see
    /// the node emitting EXTERNALIZE for recent slots and mark it as validating.
    /// The fast-forward path records the externalized value but doesn't create
    /// or emit an SCP envelope — this method fills that gap.
    /// Record an externalized value.
    pub fn record_externalized(&self, slot: SlotIndex, value: Value) {
        // Parse the StellarValue and extract stellar_value_ext for logging
        let (tx_set_hash, close_time, stellar_value_ext_desc) =
            if let Ok(sv) = StellarValue::from_xdr(&value, stellar_xdr::curr::Limits::none()) {
                let ext_desc = describe_stellar_value_ext(&sv.ext);
                (
                    Some(Hash256::from_bytes(sv.tx_set_hash.0)),
                    sv.close_time.0,
                    ext_desc,
                )
            } else {
                (None, 0, "Unknown".to_string())
            };

        // Check if we're overwriting an existing externalized value with different content
        {
            let existing = self.externalized.read();
            if let Some(old) = existing.get(&slot) {
                if old.value != value {
                    // Parse old value's stellar_value_ext for comparison
                    let old_ext_desc = if let Ok(old_sv) =
                        StellarValue::from_xdr(&old.value, stellar_xdr::curr::Limits::none())
                    {
                        describe_stellar_value_ext(&old_sv.ext)
                    } else {
                        "Unknown".to_string()
                    };
                    warn!(
                        slot,
                        old_stellar_value_ext = %old_ext_desc,
                        new_stellar_value_ext = %stellar_value_ext_desc,
                        "Overwriting externalized value with DIFFERENT value - this may cause hash mismatch!"
                    );
                }
            }
        }

        let externalized = ExternalizedSlot {
            slot,
            value,
            tx_set_hash,
            close_time,
            externalized_at: std::time::Instant::now(),
        };

        self.externalized.write().insert(slot, externalized);

        // Update latest
        let mut latest = self.latest_externalized.write();
        if latest.map(|l| slot > l).unwrap_or(true) {
            *latest = Some(slot);
        }

        debug!("Externalized slot {} with close time {}", slot, close_time);
    }

    /// Get the close time of an externalized slot.
    pub fn get_externalized_close_time(&self, slot: SlotIndex) -> Option<u64> {
        self.externalized.read().get(&slot).map(|e| e.close_time)
    }

    /// Emit an envelope to the network.
    fn emit(&self, envelope: ScpEnvelope) {
        if let Some(sender) = self.envelope_sender.read().as_ref() {
            sender(envelope);
        }
    }

    /// Get the cache size.
    pub fn tx_set_cache_size(&self) -> usize {
        self.tx_set_cache.len()
    }

    /// Get the pending tx sets count.
    pub fn pending_tx_sets_size(&self) -> usize {
        self.pending_tx_sets.len()
    }

    /// Get the pending quorum sets count.
    pub fn pending_quorum_sets_size(&self) -> usize {
        self.pending_quorum_sets.len()
    }

    /// Get the externalized slots count.
    pub fn externalized_size(&self) -> usize {
        self.externalized.read().len()
    }

    /// Get the quorum sets count (by node ID).
    pub fn quorum_sets_size(&self) -> usize {
        self.quorum_sets.len()
    }

    /// Get the quorum sets by hash count.
    pub fn quorum_sets_by_hash_size(&self) -> usize {
        self.quorum_sets_by_hash.len()
    }

    /// Get all cache sizes for diagnostics.
    pub fn cache_sizes(&self) -> ScpDriverCacheSizes {
        ScpDriverCacheSizes {
            tx_set_cache: self.tx_set_cache.len(),
            pending_tx_sets: self.pending_tx_sets.len(),
            pending_quorum_sets: self.pending_quorum_sets.len(),
            externalized: self.externalized.read().len(),
            quorum_sets: self.quorum_sets.len(),
            quorum_sets_by_hash: self.quorum_sets_by_hash.len(),
        }
    }

    /// Clear old externalized slots.
    pub fn cleanup_externalized(&self, keep_count: usize) {
        let mut externalized = self.externalized.write();
        if externalized.len() <= keep_count {
            return;
        }

        // Get slots sorted by slot index
        let mut slots: Vec<SlotIndex> = externalized.keys().copied().collect();
        slots.sort();

        // Remove oldest
        let to_remove = externalized.len() - keep_count;
        for slot in slots.into_iter().take(to_remove) {
            externalized.remove(&slot);
        }
    }

    /// Clear the transaction set cache.
    pub fn clear_tx_set_cache(&self) {
        let count = self.tx_set_cache.len();
        self.tx_set_cache.clear();
        if count > 0 {
            tracing::info!(count, "Cleared scp_driver tx_set_cache");
        }
    }

    /// Clear all caches in scp_driver.
    /// Called after catchup to release stale cached data.
    pub fn clear_all_caches(&self) {
        let tx_set_count = self.tx_set_cache.len();
        let pending_count = self.pending_tx_sets.len();
        let externalized_count = self.externalized.read().len();
        let qs_count = self.quorum_sets.len();
        let qs_hash_count = self.quorum_sets_by_hash.len();

        self.tx_set_cache.clear();
        self.pending_tx_sets.clear();
        self.externalized.write().clear();

        // Clear quorum set caches, preserving only the local node's entry.
        let local_key: [u8; 32] = *self.config.node_id.as_bytes();
        let local_qs = self.local_quorum_set.read().clone();
        self.quorum_sets.clear();
        self.quorum_sets_by_hash.clear();
        if let Some(qs) = local_qs {
            let hash = hash_quorum_set(&qs);
            self.quorum_sets.insert(local_key, qs.clone());
            self.quorum_sets_by_hash.insert(hash, qs);
        }

        if tx_set_count > 0 || pending_count > 0 || externalized_count > 0 || qs_count > 1 {
            tracing::info!(
                tx_set_count,
                pending_count,
                externalized_count,
                qs_count,
                qs_hash_count,
                "Cleared scp_driver caches"
            );
        }
    }

    /// Trim stale caches while preserving data for slots after catchup.
    /// Called after catchup to release memory while keeping tx_sets and
    /// pending requests that will be needed for buffered ledgers.
    pub fn trim_stale_caches(&self, keep_after_slot: SlotIndex) {
        let initial_pending_count = self.pending_tx_sets.len();
        let initial_externalized_count = self.externalized.read().len();

        // Trim pending_tx_sets - keep requests for slots > keep_after_slot
        self.pending_tx_sets
            .retain(|_, pending| pending.slot > keep_after_slot);

        // Trim externalized - keep slots > keep_after_slot
        {
            let mut externalized = self.externalized.write();
            externalized.retain(|slot, _| *slot > keep_after_slot);
        }

        // Note: we don't trim tx_set_cache because it's keyed by hash, not slot.
        // The tx_sets we need are also cached in fetching_envelopes which we DO trim.
        // The tx_set_cache is small and will be naturally evicted by max size limits.

        let kept_pending = self.pending_tx_sets.len();
        let kept_externalized = self.externalized.read().len();

        tracing::info!(
            initial_pending_count,
            initial_externalized_count,
            kept_pending,
            kept_externalized,
            keep_after_slot,
            "Trimmed stale scp_driver caches, preserving future slots"
        );
    }

    /// Purge SCP state for slots below the given slot.
    ///
    /// This removes externalized slots and cached tx sets for old slots,
    /// freeing memory during out-of-sync recovery.
    pub fn purge_slots_below(&self, slot: SlotIndex) {
        // Remove externalized slots below the threshold
        let mut externalized = self.externalized.write();
        let slots_to_remove: Vec<_> = externalized
            .keys()
            .filter(|&s| *s < slot)
            .cloned()
            .collect();
        for s in slots_to_remove {
            externalized.remove(&s);
        }
        drop(externalized);

        // Clean up pending tx set requests for old slots
        self.cleanup_old_pending_slots(slot);

        // Clean up quorum set caches to prevent unbounded growth.
        // In stellar-core these use weak_ptr and expire automatically;
        // here we clear and re-insert only the local node's quorum set.
        // Missing entries will be re-fetched from peers on demand via
        // the pending_quorum_sets mechanism.
        let local_key: [u8; 32] = *self.config.node_id.as_bytes();
        let local_qs = self.local_quorum_set.read().clone();

        let prev_by_node = self.quorum_sets.len();
        let prev_by_hash = self.quorum_sets_by_hash.len();

        self.quorum_sets.clear();
        self.quorum_sets_by_hash.clear();

        if let Some(qs) = local_qs {
            let hash = hash_quorum_set(&qs);
            self.quorum_sets.insert(local_key, qs.clone());
            self.quorum_sets_by_hash.insert(hash, qs);
        }

        if prev_by_node > 1 || prev_by_hash > 1 {
            tracing::debug!(
                prev_by_node,
                prev_by_hash,
                "Cleared quorum set caches during slot purge"
            );
        }
    }

    /// Get local SCP envelopes for a slot.
    ///
    /// Returns envelopes this node has emitted for the given slot.
    /// Note: Currently returns empty since we don't store envelopes in ExternalizedSlot.
    /// This can be enhanced to store and return actual envelopes if needed for recovery.
    pub fn get_local_envelopes(&self, _slot: SlotIndex) -> Vec<ScpEnvelope> {
        // ExternalizedSlot doesn't store the envelope, just the value.
        // In a full implementation, we'd store envelopes separately.
        Vec::new()
    }

    /// Get our local quorum set.
    pub fn get_local_quorum_set(&self) -> Option<ScpQuorumSet> {
        self.local_quorum_set.read().clone()
    }

    /// Set our local quorum set.
    pub fn set_local_quorum_set(&self, quorum_set: ScpQuorumSet) {
        *self.local_quorum_set.write() = Some(quorum_set);
        if let Some(local) = self.local_quorum_set.read().clone() {
            let hash = hash_quorum_set(&local);
            self.quorum_sets_by_hash.insert(hash, local.clone());
            self.quorum_sets
                .insert(*self.config.node_id.as_bytes(), local);
            self.pending_quorum_sets.remove(&hash);
        }
    }

    /// Store a quorum set for a node.
    pub fn store_quorum_set(&self, node_id: &stellar_xdr::curr::NodeId, quorum_set: ScpQuorumSet) {
        let key: [u8; 32] = match &node_id.0 {
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key) => key.0,
        };
        let hash = hash_quorum_set(&quorum_set);
        self.quorum_sets.insert(key, quorum_set.clone());
        self.quorum_sets_by_hash.insert(hash, quorum_set);
        self.pending_quorum_sets.remove(&hash);
    }

    /// Get a quorum set for a node.
    pub fn get_quorum_set(&self, node_id: &stellar_xdr::curr::NodeId) -> Option<ScpQuorumSet> {
        let key: [u8; 32] = match &node_id.0 {
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key) => key.0,
        };

        // Check if this is our own node
        let our_key: [u8; 32] = *self.config.node_id.as_bytes();
        if key == our_key {
            return self.local_quorum_set.read().clone();
        }

        self.quorum_sets.get(&key).map(|v| v.clone())
    }

    /// Get a quorum set by its hash.
    pub fn get_quorum_set_by_hash(&self, hash: &Hash256) -> Option<ScpQuorumSet> {
        self.quorum_sets_by_hash.get(hash).map(|v| v.clone())
    }

    /// Whether we already have a quorum set with the given hash.
    pub fn has_quorum_set_hash(&self, hash: &Hash256) -> bool {
        self.quorum_sets_by_hash.contains_key(hash)
    }

    /// Get our node ID.
    pub fn node_id(&self) -> &PublicKey {
        &self.config.node_id
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::tx_queue::TransactionSet;
    use henyey_scp::hash_quorum_set;
    use std::thread;
    use std::time::Duration;

    fn make_config(max_cache: usize) -> ScpDriverConfig {
        ScpDriverConfig {
            max_tx_set_cache: max_cache,
            ..ScpDriverConfig::default()
        }
    }

    fn make_tx_set(seed: u8) -> TransactionSet {
        let prev_hash = Hash256::from_bytes([seed; 32]);
        TransactionSet::new(prev_hash, Vec::new())
    }

    #[test]
    fn test_cache_tx_set_evicts_oldest() {
        let driver = ScpDriver::new(make_config(1), Hash256::hash(b"network"));
        let first = make_tx_set(1);
        let second = make_tx_set(2);

        driver.cache_tx_set(first.clone());
        thread::sleep(Duration::from_millis(1));
        driver.cache_tx_set(second.clone());

        assert!(!driver.has_tx_set(&first.hash));
        assert!(driver.has_tx_set(&second.hash));
    }

    #[test]
    fn test_request_and_receive_tx_set() {
        let driver = ScpDriver::new(make_config(4), Hash256::hash(b"network"));
        let tx_set = make_tx_set(3);
        let slot = 12u64;

        assert!(driver.request_tx_set(tx_set.hash, slot));
        assert!(!driver.request_tx_set(tx_set.hash, slot));
        assert!(driver.needs_tx_set(&tx_set.hash));
        assert_eq!(driver.get_pending_tx_set_hashes(), vec![tx_set.hash]);

        let received = driver.receive_tx_set(tx_set.clone());
        assert_eq!(received, Some(slot));
        assert!(!driver.needs_tx_set(&tx_set.hash));
        assert!(driver.get_tx_set(&tx_set.hash).is_some());
    }

    #[test]
    fn test_receive_tx_set_rejects_mismatched_hash() {
        let driver = ScpDriver::new(make_config(2), Hash256::hash(b"network"));
        let tx_set = make_tx_set(4);
        let bad_hash = Hash256::from_bytes([9; 32]);
        let bad_set = TransactionSet::with_hash(tx_set.previous_ledger_hash, bad_hash, Vec::new());

        let received = driver.receive_tx_set(bad_set);
        assert_eq!(received, None);
        assert!(!driver.has_tx_set(&bad_hash));
    }

    #[test]
    fn test_cleanup_old_pending_slots() {
        let driver = ScpDriver::new(make_config(4), Hash256::hash(b"network"));
        let tx_set_a = make_tx_set(5);
        let tx_set_b = make_tx_set(6);

        driver.request_tx_set(tx_set_a.hash, 10);
        driver.request_tx_set(tx_set_b.hash, 12);

        let removed = driver.cleanup_old_pending_slots(12);
        assert_eq!(removed, 1);

        let pending = driver.get_pending_tx_sets();
        assert_eq!(pending, vec![(tx_set_b.hash, 12)]);
    }

    #[test]
    fn test_cleanup_pending_tx_sets_by_age() {
        let driver = ScpDriver::new(make_config(4), Hash256::hash(b"network"));
        let tx_set = make_tx_set(7);
        driver.request_tx_set(tx_set.hash, 20);

        driver.cleanup_pending_tx_sets(0);
        assert!(driver.get_pending_tx_set_hashes().is_empty());
    }

    #[test]
    fn test_request_quorum_set_tracks_unknown_only() {
        let node_id = PublicKey::from_bytes(&[9u8; 32]).expect("node id");
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![stellar_xdr::curr::NodeId(
                stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(stellar_xdr::curr::Uint256(
                    [1u8; 32],
                )),
            )]
            .try_into()
            .unwrap(),
            inner_sets: Vec::new().try_into().unwrap(),
        };
        let config = ScpDriverConfig {
            node_id,
            local_quorum_set: Some(quorum_set.clone()),
            ..make_config(4)
        };
        let driver = ScpDriver::new(config, Hash256::hash(b"network"));

        // Create a test node_id for the request
        let sender_node_id =
            stellar_xdr::curr::NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                stellar_xdr::curr::Uint256([2u8; 32]),
            ));

        let known_hash = hash_quorum_set(&quorum_set);
        assert!(!driver.request_quorum_set(known_hash, sender_node_id.clone()));

        let unknown_hash = Hash256::from_bytes([42u8; 32]);
        assert!(driver.request_quorum_set(unknown_hash, sender_node_id.clone()));
        assert!(!driver.request_quorum_set(unknown_hash, sender_node_id.clone()));

        // Verify the node_id was tracked
        let pending_ids = driver.get_pending_quorum_set_node_ids(&unknown_hash);
        assert_eq!(pending_ids.len(), 1);
        assert_eq!(pending_ids[0], sender_node_id);
    }

    fn make_externalized_slot(slot: SlotIndex, close_time: u64) -> ExternalizedSlot {
        ExternalizedSlot {
            slot,
            value: Value(vec![].try_into().unwrap()),
            tx_set_hash: Some(Hash256::from_bytes([slot as u8; 32])),
            close_time,
            externalized_at: std::time::Instant::now(),
        }
    }

    #[test]
    fn test_get_externalized_slots_in_range() {
        let driver = ScpDriver::new(make_config(4), Hash256::hash(b"network"));

        // Externalize some slots (manually insert into the map for testing)
        {
            let mut externalized = driver.externalized.write();
            externalized.insert(100, make_externalized_slot(100, 1000));
            externalized.insert(102, make_externalized_slot(102, 1010));
            externalized.insert(105, make_externalized_slot(105, 1020));
            externalized.insert(110, make_externalized_slot(110, 1030));
        }

        // Test exact range
        let slots = driver.get_externalized_slots_in_range(100, 105);
        assert_eq!(slots, vec![100, 102, 105]);

        // Test partial range
        let slots = driver.get_externalized_slots_in_range(101, 106);
        assert_eq!(slots, vec![102, 105]);

        // Test empty range (no slots in range)
        let slots = driver.get_externalized_slots_in_range(106, 109);
        assert!(slots.is_empty());

        // Test single slot
        let slots = driver.get_externalized_slots_in_range(102, 102);
        assert_eq!(slots, vec![102]);
    }

    #[test]
    fn test_find_missing_slots_in_range() {
        let driver = ScpDriver::new(make_config(4), Hash256::hash(b"network"));

        // Externalize some slots with gaps
        {
            let mut externalized = driver.externalized.write();
            externalized.insert(100, make_externalized_slot(100, 1000));
            externalized.insert(102, make_externalized_slot(102, 1010));
            externalized.insert(105, make_externalized_slot(105, 1020));
        }

        // Find missing slots in range 100-105
        let missing = driver.find_missing_slots_in_range(100, 105);
        assert_eq!(missing, vec![101, 103, 104]);

        // No missing slots when all present
        let missing = driver.find_missing_slots_in_range(100, 100);
        assert!(missing.is_empty());

        // All slots missing
        let missing = driver.find_missing_slots_in_range(106, 108);
        assert_eq!(missing, vec![106, 107, 108]);

        // Invalid range (from > to)
        let missing = driver.find_missing_slots_in_range(110, 100);
        assert!(missing.is_empty());
    }

    #[test]
    fn test_trim_stale_caches_preserves_future_slots() {
        let driver = ScpDriver::new(make_config(10), Hash256::hash(b"network"));

        // Add pending tx_sets for various slots
        let tx_set_old = make_tx_set(1);
        let tx_set_boundary = make_tx_set(2);
        let tx_set_future1 = make_tx_set(3);
        let tx_set_future2 = make_tx_set(4);

        driver.request_tx_set(tx_set_old.hash, 98);
        driver.request_tx_set(tx_set_boundary.hash, 100);
        driver.request_tx_set(tx_set_future1.hash, 101);
        driver.request_tx_set(tx_set_future2.hash, 105);

        // Add externalized slots
        {
            let mut externalized = driver.externalized.write();
            externalized.insert(95, make_externalized_slot(95, 1000));
            externalized.insert(100, make_externalized_slot(100, 1010));
            externalized.insert(101, make_externalized_slot(101, 1020));
            externalized.insert(105, make_externalized_slot(105, 1030));
        }

        // Trim with keep_after_slot = 100
        // Should keep slots > 100, i.e., 101 and 105
        driver.trim_stale_caches(100);

        // Verify pending_tx_sets
        let pending = driver.get_pending_tx_sets();
        assert_eq!(pending.len(), 2);
        assert!(pending
            .iter()
            .any(|(h, s)| *h == tx_set_future1.hash && *s == 101));
        assert!(pending
            .iter()
            .any(|(h, s)| *h == tx_set_future2.hash && *s == 105));
        // Old and boundary slots should be removed
        assert!(!pending.iter().any(|(h, _)| *h == tx_set_old.hash));
        assert!(!pending.iter().any(|(h, _)| *h == tx_set_boundary.hash));

        // Verify externalized slots
        let ext_slots = driver.get_externalized_slots_in_range(0, 200);
        assert_eq!(ext_slots, vec![101, 105]);
    }

    #[test]
    fn test_clear_pending_tx_sets_removes_all() {
        let driver = ScpDriver::new(make_config(4), Hash256::hash(b"network"));
        let tx_set_a = make_tx_set(10);
        let tx_set_b = make_tx_set(11);
        let tx_set_c = make_tx_set(12);

        driver.request_tx_set(tx_set_a.hash, 100);
        driver.request_tx_set(tx_set_b.hash, 101);
        driver.request_tx_set(tx_set_c.hash, 102);
        assert_eq!(driver.get_pending_tx_sets().len(), 3);

        driver.clear_pending_tx_sets();
        assert!(driver.get_pending_tx_sets().is_empty());
        assert!(driver.get_pending_tx_set_hashes().is_empty());
    }

    #[test]
    fn test_clear_pending_tx_sets_noop_when_empty() {
        let driver = ScpDriver::new(make_config(4), Hash256::hash(b"network"));
        assert!(driver.get_pending_tx_sets().is_empty());

        // Should not panic when called on empty map
        driver.clear_pending_tx_sets();
        assert!(driver.get_pending_tx_sets().is_empty());
    }

    #[test]
    fn test_clear_pending_tx_sets_does_not_affect_cache() {
        let driver = ScpDriver::new(make_config(4), Hash256::hash(b"network"));
        let tx_set = make_tx_set(20);

        // Request and then receive the tx_set (puts it in cache)
        driver.request_tx_set(tx_set.hash, 200);
        driver.receive_tx_set(tx_set.clone());
        assert!(driver.has_tx_set(&tx_set.hash));

        // Add another pending request
        let tx_set_b = make_tx_set(21);
        driver.request_tx_set(tx_set_b.hash, 201);

        // Clear pending — should not affect the cached tx_set
        driver.clear_pending_tx_sets();
        assert!(driver.has_tx_set(&tx_set.hash));
        assert!(driver.get_tx_set(&tx_set.hash).is_some());
        assert!(driver.get_pending_tx_sets().is_empty());
    }

    /// Create a signed StellarValue for testing validate_value_impl.
    fn make_signed_stellar_value(
        network_id: &Hash256,
        secret: &henyey_crypto::SecretKey,
        tx_set_hash: [u8; 32],
        close_time: u64,
    ) -> stellar_xdr::curr::Value {
        use stellar_xdr::curr::{
            EnvelopeType, LedgerCloseValueSignature, Limits, NodeId as XdrNodeId, StellarValue,
            StellarValueExt, TimePoint, WriteXdr,
        };

        let xdr_tx_set_hash = stellar_xdr::curr::Hash(tx_set_hash);
        let ct = TimePoint(close_time);

        // Sign: (networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)
        let mut sign_data = network_id.0.to_vec();
        sign_data.extend_from_slice(&EnvelopeType::Scpvalue.to_xdr(Limits::none()).unwrap());
        sign_data.extend_from_slice(&xdr_tx_set_hash.to_xdr(Limits::none()).unwrap());
        sign_data.extend_from_slice(&ct.to_xdr(Limits::none()).unwrap());
        let sig = secret.sign(&sign_data);

        let node_id = XdrNodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256(*secret.public_key().as_bytes()),
        ));

        let sv = StellarValue {
            tx_set_hash: xdr_tx_set_hash,
            close_time: ct,
            upgrades: vec![].try_into().unwrap(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id,
                signature: stellar_xdr::curr::Signature(
                    sig.0.to_vec().try_into().unwrap_or_default(),
                ),
            }),
        };
        stellar_xdr::curr::Value(sv.to_xdr(Limits::none()).unwrap().try_into().unwrap())
    }

    #[test]
    fn test_validate_value_returns_invalid_for_missing_tx_set() {
        // When validating a value for LCL+1 (current ledger) and the tx set is
        // not cached, validate_value should return Invalid (matching stellar-core).
        let secret = henyey_crypto::SecretKey::from_seed(&[42u8; 32]);
        let network_id = Hash256::hash(b"test-network");

        let config = ScpDriverConfig {
            node_id: secret.public_key(),
            ..make_config(4)
        };
        let driver = ScpDriver::new(config, network_id);

        // Set tracking state: LCL is at slot 0, so slot 1 is LCL+1 (current ledger)
        driver.set_tracking_state(true, 1, 1);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let tx_set_hash = [99u8; 32];

        // Create a signed value with a tx_set_hash that is NOT cached
        let value = make_signed_stellar_value(&network_id, &secret, tx_set_hash, now);

        let result = driver.validate_value_impl(1, &value);
        assert_eq!(
            result,
            ValueValidation::Invalid,
            "missing tx set for current ledger should return Invalid, not MaybeValid"
        );

        // Now cache the tx set and re-validate — should pass the tx set check
        let tx_set = TransactionSet::new(Hash256::ZERO, Vec::new());
        // The tx_set created by TransactionSet::new computes its own hash,
        // so we need to use that hash in the value
        let tx_set_hash_real = tx_set.hash;
        driver.cache_tx_set(tx_set);

        let value2 = make_signed_stellar_value(&network_id, &secret, tx_set_hash_real.0, now);
        let result2 = driver.validate_value_impl(1, &value2);
        // Should NOT be Invalid (the tx set is now available)
        // It may be MaybeValid or Valid depending on other checks
        assert_ne!(
            result2,
            ValueValidation::Invalid,
            "with tx set cached, validation should not return Invalid"
        );
    }
}

/// SCP callback implementation wrapper.
///
/// This wraps the ScpDriver to implement the SCPDriver trait.
pub struct HerderScpCallback {
    driver: Arc<ScpDriver>,
}

impl HerderScpCallback {
    /// Create a new callback wrapper.
    pub fn new(driver: Arc<ScpDriver>) -> Self {
        Self { driver }
    }

    fn hash_helper<F>(&self, slot_index: u64, prev_value: &Value, extra: F) -> u64
    where
        F: FnOnce(&mut Vec<Vec<u8>>),
    {
        let mut values = Vec::new();
        values.push(Self::xdr_bytes(&slot_index));
        values.push(Self::xdr_bytes(prev_value));
        extra(&mut values);

        let mut data = Vec::new();
        for value in values {
            data.extend_from_slice(&value);
        }
        let hash = Hash256::hash(&data);
        let mut result = 0u64;
        for byte in &hash.as_bytes()[0..8] {
            result = (result << 8) | (*byte as u64);
        }
        result
    }

    fn xdr_bytes<T: stellar_xdr::curr::WriteXdr>(value: &T) -> Vec<u8> {
        value
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap_or_default()
    }
}

impl SCPDriver for HerderScpCallback {
    fn validate_value(&self, slot_index: u64, value: &Value, nomination: bool) -> ValidationLevel {
        // Parity: stellar-core's HerderSCPDriver::validateValue returns
        // kFullyValidatedValue for ballot protocol (nomination=false).
        // During the ballot protocol (PREPARE/CONFIRM/EXTERNALIZE), the
        // value was already validated during nomination — re-validation
        // would fail for future slots whose close times are ahead of our
        // local state. This is critical for post-catchup convergence:
        // EXTERNALIZE envelopes from the current network slot must be
        // accepted without close-time or tx-set validation.
        if !nomination {
            return ValidationLevel::FullyValidated;
        }
        match self.driver.validate_value_impl(slot_index, value) {
            ValueValidation::Valid => ValidationLevel::FullyValidated,
            ValueValidation::MaybeValid => ValidationLevel::MaybeValid,
            ValueValidation::Invalid => ValidationLevel::Invalid,
        }
    }

    fn combine_candidates(&self, slot_index: u64, candidates: &[Value]) -> Option<Value> {
        let result = self.driver.combine_candidates_impl(slot_index, candidates);
        if result.0.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    fn extract_valid_value(&self, slot_index: u64, value: &Value) -> Option<Value> {
        self.driver.extract_valid_value_impl(slot_index, value)
    }

    fn emit_envelope(&self, envelope: &ScpEnvelope) {
        self.driver.emit(envelope.clone());
    }

    fn get_quorum_set(&self, node_id: &stellar_xdr::curr::NodeId) -> Option<ScpQuorumSet> {
        self.driver.get_quorum_set(node_id)
    }

    fn get_quorum_set_by_hash(&self, hash: &henyey_common::Hash256) -> Option<ScpQuorumSet> {
        self.driver.get_quorum_set_by_hash(hash)
    }

    fn nominating_value(&self, _slot_index: u64, _value: &Value) {
        // Logging only
    }

    fn value_externalized(&self, slot_index: u64, value: &Value) {
        self.driver.record_externalized(slot_index, value.clone());
    }

    fn ballot_did_prepare(&self, _slot_index: u64, _ballot: &stellar_xdr::curr::ScpBallot) {
        // Logging only
    }

    fn ballot_did_confirm(&self, _slot_index: u64, _ballot: &stellar_xdr::curr::ScpBallot) {
        // Logging only
    }

    fn compute_hash_node(
        &self,
        slot_index: u64,
        prev_value: &Value,
        is_priority: bool,
        round: u32,
        node_id: &stellar_xdr::curr::NodeId,
    ) -> u64 {
        const HASH_N: u32 = 1;
        const HASH_P: u32 = 2;
        self.hash_helper(slot_index, prev_value, |values| {
            let tag = if is_priority { HASH_P } else { HASH_N };
            values.push(Self::xdr_bytes(&tag));
            values.push(Self::xdr_bytes(&round));
            values.push(Self::xdr_bytes(node_id));
        })
    }

    fn compute_value_hash(
        &self,
        slot_index: u64,
        prev_value: &Value,
        round: u32,
        value: &Value,
    ) -> u64 {
        const HASH_K: u32 = 3;
        self.hash_helper(slot_index, prev_value, |values| {
            values.push(Self::xdr_bytes(&HASH_K));
            values.push(Self::xdr_bytes(&round));
            values.push(Self::xdr_bytes(value));
        })
    }

    fn compute_timeout(&self, round: u32, is_nomination: bool) -> std::time::Duration {
        const MAX_TIMEOUT_MS: u64 = 30 * 60 * 1000;
        let mut initial_ms: u64 = 1000;
        let mut increment_ms: u64 = 1000;
        if let Some(manager) = self.driver.ledger_manager.read().as_ref() {
            let header = manager.current_header();
            if protocol_version_starts_from(header.ledger_version, ProtocolVersion::V23) {
                if let Some(info) = manager.soroban_network_info() {
                    if is_nomination {
                        initial_ms = info.nomination_timeout_initial_ms as u64;
                        increment_ms = info.nomination_timeout_increment_ms as u64;
                    } else {
                        initial_ms = info.ballot_timeout_initial_ms as u64;
                        increment_ms = info.ballot_timeout_increment_ms as u64;
                    }
                }
            }
        }
        let round = round.max(1) as u64;
        let timeout_ms = initial_ms.saturating_add((round - 1).saturating_mul(increment_ms));
        std::time::Duration::from_millis(timeout_ms.min(MAX_TIMEOUT_MS))
    }

    fn sign_envelope(&self, envelope: &mut ScpEnvelope) {
        if let Some(sig) = self.driver.sign_envelope(&envelope.statement) {
            envelope.signature =
                stellar_xdr::curr::Signature(sig.0.to_vec().try_into().unwrap_or_default());
        }
    }

    fn verify_envelope(&self, envelope: &ScpEnvelope) -> bool {
        self.driver.verify_envelope(envelope).is_ok()
    }

    /// Parity: check if a value contains protocol upgrades.
    fn has_upgrades(&self, value: &Value) -> bool {
        if let Ok(sv) = StellarValue::from_xdr(value, stellar_xdr::curr::Limits::none()) {
            !sv.upgrades.is_empty()
        } else {
            false
        }
    }

    /// Parity: strip all upgrades from a value.
    fn strip_all_upgrades(&self, value: &Value) -> Option<Value> {
        let mut sv = StellarValue::from_xdr(value, stellar_xdr::curr::Limits::none()).ok()?;
        sv.upgrades = Vec::new().try_into().ok()?;
        sv.to_xdr(stellar_xdr::curr::Limits::none())
            .ok()
            .map(|bytes| Value(bytes.try_into().unwrap_or_default()))
    }

    /// Parity: get the nomination timeout limit for upgrade stripping.
    fn get_upgrade_nomination_timeout_limit(&self) -> u32 {
        // In stellar-core, this comes from mUpgrades.getParameters().mNominationTimeoutLimit
        // Default is u32::MAX (never strip), which matches stellar-core default
        u32::MAX
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        LedgerCloseValueSignature, Limits, StellarValue, StellarValueExt, TimePoint, UpgradeType,
        VecM,
    };

    fn make_test_driver() -> ScpDriver {
        let config = ScpDriverConfig::default();
        ScpDriver::new(config, Hash256::ZERO)
    }

    /// Create a test driver with a known secret key for signing.
    fn make_test_driver_with_key() -> (ScpDriver, SecretKey) {
        let secret_key = SecretKey::generate();
        let public_key = secret_key.public_key();
        let config = ScpDriverConfig {
            node_id: public_key,
            ..ScpDriverConfig::default()
        };
        let network_id = Hash256::ZERO;
        let driver = ScpDriver::with_secret_key(config, network_id, secret_key.clone());
        (driver, secret_key)
    }

    /// Create a properly SIGNED StellarValue using the given secret key and network ID.
    fn make_signed_stellar_value(
        tx_set_hash: stellar_xdr::curr::Hash,
        close_time: u64,
        upgrades: Vec<UpgradeType>,
        secret_key: &SecretKey,
        network_id: &Hash256,
    ) -> StellarValue {
        let close_time = TimePoint(close_time);
        // Sign: (networkID, ENVELOPE_TYPE_SCPVALUE, txSetHash, closeTime)
        let mut sign_data = network_id.0.to_vec();
        sign_data.extend_from_slice(&EnvelopeType::Scpvalue.to_xdr(Limits::none()).expect("xdr"));
        sign_data.extend_from_slice(&tx_set_hash.to_xdr(Limits::none()).expect("xdr"));
        sign_data.extend_from_slice(&close_time.to_xdr(Limits::none()).expect("xdr"));
        let sig = secret_key.sign(&sign_data);

        let node_id =
            stellar_xdr::curr::NodeId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                stellar_xdr::curr::Uint256(*secret_key.public_key().as_bytes()),
            ));

        StellarValue {
            tx_set_hash,
            close_time,
            upgrades: upgrades.try_into().unwrap_or_default(),
            ext: StellarValueExt::Signed(LedgerCloseValueSignature {
                node_id,
                signature: stellar_xdr::curr::Signature(
                    sig.0.to_vec().try_into().unwrap_or_default(),
                ),
            }),
        }
    }

    /// Encode a StellarValue to a Value.
    fn encode_sv(sv: &StellarValue) -> Value {
        Value(sv.to_xdr(Limits::none()).expect("xdr").try_into().unwrap())
    }

    #[test]
    fn test_tx_set_caching() {
        let driver = make_test_driver();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let hash = tx_set.hash;

        driver.cache_tx_set(tx_set);
        assert!(driver.has_tx_set(&hash));

        let cached = driver.get_tx_set(&hash);
        assert!(cached.is_some());
    }

    #[test]
    fn test_externalized_recording() {
        let driver = make_test_driver();

        assert!(driver.latest_externalized_slot().is_none());

        driver.record_externalized(100, Value::default());
        assert_eq!(driver.latest_externalized_slot(), Some(100));

        driver.record_externalized(99, Value::default()); // older slot
        assert_eq!(driver.latest_externalized_slot(), Some(100)); // still 100

        driver.record_externalized(101, Value::default());
        assert_eq!(driver.latest_externalized_slot(), Some(101));
    }

    #[test]
    fn test_cleanup_externalized() {
        let driver = make_test_driver();

        for slot in 1..=10 {
            driver.record_externalized(slot, Value::default());
        }

        assert_eq!(driver.externalized.read().len(), 10);

        driver.cleanup_externalized(5);
        assert_eq!(driver.externalized.read().len(), 5);

        // Should keep slots 6-10
        let externalized = driver.externalized.read();
        assert!(!externalized.contains_key(&1));
        assert!(!externalized.contains_key(&5));
        assert!(externalized.contains_key(&6));
        assert!(externalized.contains_key(&10));
    }

    #[test]
    fn test_combine_single_value() {
        let driver = make_test_driver();

        let value = Value(vec![1, 2, 3].try_into().unwrap());
        let result = driver.combine_candidates_impl(1, &[value.clone()]);
        assert_eq!(result, value);
    }

    #[test]
    fn test_combine_empty() {
        let driver = make_test_driver();

        let result = driver.combine_candidates_impl(1, &[]);
        assert_eq!(result, Value::default());
    }

    #[test]
    fn test_invalid_upgrade_rejected() {
        let (driver, secret_key) = make_test_driver_with_key();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = tx_set.hash;
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let invalid_upgrade = UpgradeType(vec![0u8; 1].try_into().unwrap());
        let sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            now,
            vec![invalid_upgrade],
            &secret_key,
            &driver.network_id,
        );
        let value = encode_sv(&sv);

        assert_eq!(
            driver.validate_value_impl(1, &value),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_tx_set_hash_mismatch_rejected() {
        let (driver, secret_key) = make_test_driver_with_key();

        let tx_set = TransactionSet::with_hash(Hash256::ZERO, Hash256::ZERO, vec![]);
        let tx_set_hash = tx_set.hash;
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            now,
            vec![],
            &secret_key,
            &driver.network_id,
        );
        let value = encode_sv(&sv);

        assert_eq!(
            driver.validate_value_impl(1, &value),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_close_time_must_increase() {
        let (driver, secret_key) = make_test_driver_with_key();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = tx_set.hash;
        driver.cache_tx_set(tx_set);

        let base_close_time = 100;
        // Record externalized with Basic ext (record_externalized doesn't validate)
        let ext_value = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(tx_set_hash.0),
            close_time: TimePoint(base_close_time),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        };
        let ext = Value(
            ext_value
                .to_xdr(Limits::none())
                .expect("xdr")
                .try_into()
                .unwrap(),
        );
        driver.record_externalized(1, ext);

        // Now try to validate a value with same close time (should fail)
        let sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            base_close_time,
            vec![],
            &secret_key,
            &driver.network_id,
        );
        let value = encode_sv(&sv);

        assert_eq!(
            driver.validate_value_impl(2, &value),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_invalid_upgrade_order_rejected() {
        let (driver, secret_key) = make_test_driver_with_key();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = tx_set.hash;
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let base_fee = LedgerUpgrade::BaseFee(200)
            .to_xdr(Limits::none())
            .expect("xdr");
        let version = LedgerUpgrade::Version(25)
            .to_xdr(Limits::none())
            .expect("xdr");
        let upgrades = vec![
            UpgradeType(base_fee.try_into().unwrap()),
            UpgradeType(version.try_into().unwrap()),
        ];

        let sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            now,
            upgrades,
            &secret_key,
            &driver.network_id,
        );
        let value = encode_sv(&sv);

        assert_eq!(
            driver.validate_value_impl(1, &value),
            ValueValidation::Invalid
        );
    }

    // =========================================================================
    // Phase 5 parity tests
    // =========================================================================

    #[test]
    fn test_validate_rejects_basic_ext() {
        // Parity: validateValue always requires STELLAR_VALUE_SIGNED
        let driver = make_test_driver();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = tx_set.hash;
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Create a value with Basic ext (no signature)
        let stellar_value = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(tx_set_hash.0),
            close_time: TimePoint(now),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        };
        let value = encode_sv(&stellar_value);

        assert_eq!(
            driver.validate_value_impl(1, &value),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_validate_rejects_bad_signature() {
        // Parity: validateValue verifies the StellarValue signature
        let (driver, secret_key) = make_test_driver_with_key();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = tx_set.hash;
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Create a signed value but tamper with the signature
        let mut sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            now,
            vec![],
            &secret_key,
            &driver.network_id,
        );
        // Tamper with the signature
        if let StellarValueExt::Signed(ref mut sig) = sv.ext {
            let mut sig_bytes = sig.signature.to_vec();
            sig_bytes[0] ^= 0xFF;
            sig.signature = sig_bytes.try_into().expect("signature bytes");
        }
        let value = encode_sv(&sv);

        assert_eq!(
            driver.validate_value_impl(1, &value),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_validate_accepts_signed_value() {
        // Parity: a properly signed StellarValue should be accepted
        let (driver, secret_key) = make_test_driver_with_key();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = tx_set.hash;
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            now,
            vec![],
            &secret_key,
            &driver.network_id,
        );
        let value = encode_sv(&sv);

        assert_eq!(
            driver.validate_value_impl(1, &value),
            ValueValidation::Valid
        );
    }

    #[test]
    fn test_extract_valid_value_requires_fully_validated() {
        // Parity: extractValidValue only returns value when
        // validateValueAgainstLocalState returns kFullyValidatedValue.
        // When tx set is missing, it returns MaybeValid -> extractValidValue returns None.
        let driver = make_test_driver();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Value with missing tx set (MaybeValid from local state check)
        let stellar_value = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([1u8; 32]),
            close_time: TimePoint(now),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        };
        let value = encode_sv(&stellar_value);

        // extractValidValue should return None since tx set is missing
        assert!(driver.extract_valid_value_impl(1, &value).is_none());
    }

    #[test]
    fn test_extract_valid_value_strips_invalid_upgrades() {
        // Parity: extractValidValue strips invalid upgrades
        let driver = make_test_driver();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = tx_set.hash;
        driver.cache_tx_set(tx_set);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Create value with an invalid upgrade + a valid upgrade
        let version = LedgerUpgrade::Version(25)
            .to_xdr(Limits::none())
            .expect("xdr");
        let base_fee = LedgerUpgrade::BaseFee(200)
            .to_xdr(Limits::none())
            .expect("xdr");
        // Wrong order: base_fee (order 1) before version (order 0) -> invalid
        let upgrades = vec![
            UpgradeType(base_fee.try_into().unwrap()),
            UpgradeType(version.try_into().unwrap()),
        ];

        let stellar_value = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash(tx_set_hash.0),
            close_time: TimePoint(now),
            upgrades: upgrades.try_into().unwrap(),
            ext: StellarValueExt::Basic,
        };
        let value = encode_sv(&stellar_value);

        // extractValidValue should strip the out-of-order upgrade
        let result = driver.extract_valid_value_impl(1, &value);
        assert!(result.is_some());

        // The result should only have the first upgrade (base_fee at order 1)
        // since version (order 0) is out of order after base_fee
        let result_sv =
            StellarValue::from_xdr(&result.unwrap(), Limits::none()).expect("decode result");
        assert_eq!(result_sv.upgrades.len(), 1);
    }

    #[test]
    fn test_combine_candidates_merges_upgrades() {
        // Parity: combineCandidates merges upgrades from ALL candidates
        let driver = make_test_driver();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Candidate 1: version upgrade
        let version = LedgerUpgrade::Version(25)
            .to_xdr(Limits::none())
            .expect("xdr");
        let sv1 = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([1u8; 32]),
            close_time: TimePoint(now),
            upgrades: vec![UpgradeType(version.try_into().unwrap())]
                .try_into()
                .unwrap(),
            ext: StellarValueExt::Basic,
        };

        // Candidate 2: base fee upgrade
        let base_fee = LedgerUpgrade::BaseFee(200)
            .to_xdr(Limits::none())
            .expect("xdr");
        let sv2 = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([2u8; 32]),
            close_time: TimePoint(now),
            upgrades: vec![UpgradeType(base_fee.try_into().unwrap())]
                .try_into()
                .unwrap(),
            ext: StellarValueExt::Basic,
        };

        let v1 = encode_sv(&sv1);
        let v2 = encode_sv(&sv2);

        let result = driver.combine_candidates_impl(1, &[v1, v2]);
        let result_sv = StellarValue::from_xdr(&result, Limits::none()).expect("decode");

        // Result should have BOTH upgrades (merged)
        assert_eq!(result_sv.upgrades.len(), 2);
    }

    #[test]
    fn test_combine_candidates_takes_max_upgrade() {
        // Parity: when multiple candidates have same upgrade type, take max
        let driver = make_test_driver();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Candidate 1: version 24
        let v24 = LedgerUpgrade::Version(24)
            .to_xdr(Limits::none())
            .expect("xdr");
        let sv1 = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([1u8; 32]),
            close_time: TimePoint(now),
            upgrades: vec![UpgradeType(v24.try_into().unwrap())]
                .try_into()
                .unwrap(),
            ext: StellarValueExt::Basic,
        };

        // Candidate 2: version 25
        let v25 = LedgerUpgrade::Version(25)
            .to_xdr(Limits::none())
            .expect("xdr");
        let sv2 = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([2u8; 32]),
            close_time: TimePoint(now),
            upgrades: vec![UpgradeType(v25.try_into().unwrap())]
                .try_into()
                .unwrap(),
            ext: StellarValueExt::Basic,
        };

        let v1 = encode_sv(&sv1);
        let v2 = encode_sv(&sv2);

        let result = driver.combine_candidates_impl(1, &[v1, v2]);
        let result_sv = StellarValue::from_xdr(&result, Limits::none()).expect("decode");

        // Should have 1 version upgrade with value 25 (max)
        assert_eq!(result_sv.upgrades.len(), 1);
        let upgrade = LedgerUpgrade::from_xdr(result_sv.upgrades[0].0.as_slice(), Limits::none())
            .expect("decode upgrade");
        assert!(matches!(upgrade, LedgerUpgrade::Version(25)));
    }

    #[test]
    fn test_has_upgrades_and_strip() {
        // Parity: has_upgrades checks sv.upgrades.empty()
        //             strip_all_upgrades clears sv.upgrades
        let driver = Arc::new(make_test_driver());
        let callback = HerderScpCallback::new(driver);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Value without upgrades
        let sv_no_upgrades = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([1u8; 32]),
            close_time: TimePoint(now),
            upgrades: VecM::default(),
            ext: StellarValueExt::Basic,
        };
        let v_no = encode_sv(&sv_no_upgrades);
        assert!(!callback.has_upgrades(&v_no));

        // Value with upgrades
        let version = LedgerUpgrade::Version(25)
            .to_xdr(Limits::none())
            .expect("xdr");
        let sv_with_upgrades = StellarValue {
            tx_set_hash: stellar_xdr::curr::Hash([1u8; 32]),
            close_time: TimePoint(now),
            upgrades: vec![UpgradeType(version.try_into().unwrap())]
                .try_into()
                .unwrap(),
            ext: StellarValueExt::Basic,
        };
        let v_yes = encode_sv(&sv_with_upgrades);
        assert!(callback.has_upgrades(&v_yes));

        // Strip upgrades
        let stripped = callback.strip_all_upgrades(&v_yes);
        assert!(stripped.is_some());
        let stripped_sv =
            StellarValue::from_xdr(&stripped.unwrap(), Limits::none()).expect("decode");
        assert!(stripped_sv.upgrades.is_empty());
        // txSetHash and closeTime should be preserved
        assert_eq!(stripped_sv.tx_set_hash, sv_with_upgrades.tx_set_hash);
        assert_eq!(stripped_sv.close_time, sv_with_upgrades.close_time);
    }

    // =========================================================================
    // Phase 6 H1 parity tests — close-time validation
    // =========================================================================

    #[test]
    fn test_check_close_time_valid() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Valid: close time is after last close time and within max_time_drift
        assert!(driver.check_close_time(1, now - 10, now));
        assert!(driver.check_close_time(1, now - 1, now));
    }

    #[test]
    fn test_check_close_time_rejects_not_after_last() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Invalid: close time is equal to last close time
        assert!(!driver.check_close_time(1, now, now));
        // Invalid: close time is before last close time
        assert!(!driver.check_close_time(1, now, now - 1));
    }

    #[test]
    fn test_check_close_time_rejects_too_far_future() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Invalid: close time too far in future (max_time_drift defaults to MAX_TIME_SLIP_SECONDS = 60)
        assert!(!driver.check_close_time(1, now - 1, now + 120));
    }

    #[test]
    fn test_validate_past_or_future_value_same_as_lcl() {
        let driver = make_test_driver();

        // slot_index == lcl_seq: close time must match LCL exactly
        assert_eq!(
            driver.validate_past_or_future_value(
                100,
                500,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: 500,
                    tracking: None,
                }
            ),
            ValueValidation::MaybeValid
        );
        // Close time doesn't match -> Invalid
        assert_eq!(
            driver.validate_past_or_future_value(
                100,
                501,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: 500,
                    tracking: None,
                }
            ),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_validate_past_or_future_value_older_than_lcl() {
        let driver = make_test_driver();

        // slot_index < lcl_seq: close time must be strictly less than LCL
        assert_eq!(
            driver.validate_past_or_future_value(
                99,
                499,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: 500,
                    tracking: None,
                }
            ),
            ValueValidation::MaybeValid
        );
        // Close time >= LCL -> Invalid
        assert_eq!(
            driver.validate_past_or_future_value(
                99,
                500,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: 500,
                    tracking: None,
                }
            ),
            ValueValidation::Invalid
        );
        assert_eq!(
            driver.validate_past_or_future_value(
                99,
                501,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: 500,
                    tracking: None,
                }
            ),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_validate_past_or_future_value_future_not_tracking() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // slot_index > lcl_seq + 1: delegates to check_close_time, then MaybeValid
        assert_eq!(
            driver.validate_past_or_future_value(
                200,
                now, // close_time
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: now - 10,
                    tracking: None,
                }
            ),
            ValueValidation::MaybeValid
        );
    }

    #[test]
    fn test_validate_past_or_future_value_tracking_moved_on() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Tracking and tracking_index > slot_index -> already moved on -> MaybeValid
        assert_eq!(
            driver.validate_past_or_future_value(
                150,
                now,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: now - 50,
                    tracking: Some(TrackingState {
                        index: 200,
                        close_time: now - 5,
                    }),
                }
            ),
            ValueValidation::MaybeValid
        );
    }

    #[test]
    fn test_validate_past_or_future_value_tracking_future_msg() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Tracking and tracking_index < slot_index -> future message -> Invalid
        assert_eq!(
            driver.validate_past_or_future_value(
                200,
                now,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: now - 50,
                    tracking: Some(TrackingState {
                        index: 150,
                        close_time: now - 5,
                    }),
                }
            ),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_validate_past_or_future_value_tracking_same_slot() {
        let driver = make_test_driver();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Tracking and tracking_index == slot_index -> re-check with tracking close time
        assert_eq!(
            driver.validate_past_or_future_value(
                150,
                now, // close_time > tracking_close_time, within drift
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: now - 50,
                    tracking: Some(TrackingState {
                        index: 150,
                        close_time: now - 5,
                    }),
                }
            ),
            ValueValidation::MaybeValid
        );
        // If close_time <= tracking_close_time -> Invalid
        assert_eq!(
            driver.validate_past_or_future_value(
                150,
                now - 10, // close_time <= tracking_close_time
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: now - 50,
                    tracking: Some(TrackingState {
                        index: 150,
                        close_time: now - 5,
                    }),
                }
            ),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_validate_past_or_future_value_rejects_current_ledger() {
        let driver = make_test_driver();

        // slot_index == lcl_seq + 1 is the current ledger path -- should be Invalid
        // (validate_past_or_future_value is not for the current ledger)
        assert_eq!(
            driver.validate_past_or_future_value(
                101,
                500,
                ValueValidationContext {
                    lcl_seq: 100,
                    lcl_close_time: 490,
                    tracking: None,
                }
            ),
            ValueValidation::Invalid
        );
    }

    #[test]
    fn test_validate_value_against_local_state_current_ledger() {
        // When slot_index == lcl_seq + 1, validate_value_against_local_state
        // does full validation including tx set hash check
        let (driver, secret_key) = make_test_driver_with_key();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        let tx_set_hash = tx_set.hash;
        driver.cache_tx_set(tx_set);

        let sv = make_signed_stellar_value(
            stellar_xdr::curr::Hash(tx_set_hash.0),
            now,
            vec![],
            &secret_key,
            &driver.network_id,
        );

        // slot 1 == lcl_seq(0) + 1 -> current ledger path
        let result = driver.validate_value_against_local_state(1, &sv);
        assert_eq!(result, ValueValidation::Valid);
    }

    // =========================================================================
    // Phase 2A: is_tx_set_well_formed tests
    // =========================================================================

    fn make_simple_tx(seed: u8) -> stellar_xdr::curr::TransactionEnvelope {
        use stellar_xdr::curr::{
            CreateAccountOp, DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody,
            Preconditions, SequenceNumber, SignatureHint, Transaction, TransactionEnvelope,
            TransactionExt, TransactionV1Envelope, Uint256,
        };
        let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
        let dest = stellar_xdr::curr::AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256([seed.wrapping_add(1); 32])),
        );
        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(seed as i64),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    destination: dest,
                    starting_balance: 1_000_000_000,
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    #[test]
    fn test_is_tx_set_well_formed_empty() {
        let tx_set = TransactionSet::new(Hash256::ZERO, vec![]);
        assert!(ScpDriver::is_tx_set_well_formed(&tx_set));
    }

    #[test]
    fn test_is_tx_set_well_formed_single_tx() {
        let tx = make_simple_tx(1);
        let tx_set = TransactionSet::new(Hash256::ZERO, vec![tx]);
        assert!(ScpDriver::is_tx_set_well_formed(&tx_set));
    }

    #[test]
    fn test_is_tx_set_well_formed_sorted() {
        // Create txs and sort them by hash
        let mut txs: Vec<stellar_xdr::curr::TransactionEnvelope> =
            (1..=5).map(|i| make_simple_tx(i)).collect();
        txs.sort_by(|a, b| {
            let ha = Hash256::hash_xdr(a).unwrap_or(Hash256::ZERO);
            let hb = Hash256::hash_xdr(b).unwrap_or(Hash256::ZERO);
            ha.0.cmp(&hb.0)
        });

        let tx_set = TransactionSet::new(Hash256::ZERO, txs);
        assert!(ScpDriver::is_tx_set_well_formed(&tx_set));
    }

    #[test]
    fn test_is_tx_set_well_formed_unsorted() {
        // Create txs sorted by hash, then swap first two to make unsorted
        let mut txs: Vec<stellar_xdr::curr::TransactionEnvelope> =
            (1..=5).map(|i| make_simple_tx(i)).collect();
        txs.sort_by(|a, b| {
            let ha = Hash256::hash_xdr(a).unwrap_or(Hash256::ZERO);
            let hb = Hash256::hash_xdr(b).unwrap_or(Hash256::ZERO);
            ha.0.cmp(&hb.0)
        });
        // Swap first two to guarantee out-of-order
        txs.swap(0, 1);

        // Use with_hash to bypass auto-sorting in TransactionSet::new
        let tx_set = TransactionSet::with_hash(Hash256::ZERO, Hash256::ZERO, txs);
        assert!(
            !ScpDriver::is_tx_set_well_formed(&tx_set),
            "Swapped tx set should not be well-formed"
        );
    }

    #[test]
    fn test_is_tx_set_well_formed_duplicates() {
        let tx = make_simple_tx(1);
        // Use with_hash to bypass auto-sorting/dedup in TransactionSet::new
        let tx_set = TransactionSet::with_hash(Hash256::ZERO, Hash256::ZERO, vec![tx.clone(), tx]);
        assert!(
            !ScpDriver::is_tx_set_well_formed(&tx_set),
            "Tx set with duplicate should not be well-formed"
        );
    }
}

#[cfg(test)]
mod compare_tx_sets_tests {
    use super::*;
    use crate::tx_queue::TransactionSet;
    use stellar_xdr::curr::{
        GeneralizedTransactionSet, Hash, ParallelTxsComponent, TransactionPhase, TransactionSetV1,
        TxSetComponent, TxSetComponentTxsMaybeDiscountedFee,
    };

    fn make_config() -> ScpDriverConfig {
        ScpDriverConfig::default()
    }

    fn make_driver() -> ScpDriver {
        ScpDriver::new(make_config(), Hash256::ZERO)
    }

    /// Create a simple transaction with a given fee and operation count.
    fn make_tx(seed: u8, fee: u32, ops: usize) -> stellar_xdr::curr::TransactionEnvelope {
        use stellar_xdr::curr::{
            CreateAccountOp, DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody,
            Preconditions, SequenceNumber, SignatureHint, Transaction, TransactionEnvelope,
            TransactionExt, TransactionV1Envelope, Uint256,
        };
        let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
        let dest = stellar_xdr::curr::AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256([seed.wrapping_add(1); 32])),
        );
        let operations: Vec<Operation> = (0..ops)
            .map(|_| Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    destination: dest.clone(),
                    starting_balance: 1_000_000_000,
                }),
            })
            .collect();
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(seed as i64),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn cache_tx_set(driver: &ScpDriver, tx_set: TransactionSet) -> Hash256 {
        let hash = tx_set.hash;
        driver.cache_tx_set(tx_set);
        hash
    }

    fn make_generalized_tx_set(
        tx: stellar_xdr::curr::TransactionEnvelope,
        base_fee: i64,
    ) -> TransactionSet {
        let component =
            TxSetComponent::TxsetCompTxsMaybeDiscountedFee(TxSetComponentTxsMaybeDiscountedFee {
                txs: vec![tx.clone()].try_into().unwrap(),
                base_fee: Some(base_fee),
            });

        let gen = GeneralizedTransactionSet::V1(TransactionSetV1 {
            previous_ledger_hash: Hash([0u8; 32]),
            phases: vec![
                TransactionPhase::V0(vec![component].try_into().unwrap()),
                TransactionPhase::V1(ParallelTxsComponent {
                    base_fee: Some(base_fee),
                    execution_stages: vec![].try_into().unwrap(),
                }),
            ]
            .try_into()
            .unwrap(),
        });

        let hash = Hash256::hash_xdr(&gen).unwrap();
        TransactionSet::with_generalized(Hash256::ZERO, hash, vec![tx], gen)
    }

    // =========================================================================
    // compare_tx_sets — 5-criteria comparison via combine_candidates_impl
    // =========================================================================

    #[test]
    fn test_compare_tx_sets_more_ops_wins() {
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        // Set A: 2 ops, fee 200
        let tx_set_a = TransactionSet::new(Hash256::ZERO, vec![make_tx(1, 200, 2)]);
        let hash_a = cache_tx_set(&driver, tx_set_a);

        // Set B: 1 op, fee 200
        let tx_set_b = TransactionSet::new(Hash256::ZERO, vec![make_tx(2, 200, 1)]);
        let hash_b = cache_tx_set(&driver, tx_set_b);

        // A has more ops, so A > B
        let result = driver.compare_tx_sets(&hash_a, &hash_b, &candidates_hash);
        assert_eq!(result, std::cmp::Ordering::Greater);
    }

    #[test]
    fn test_compare_tx_sets_fewer_ops_loses() {
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        // Set A: 1 op
        let tx_set_a = TransactionSet::new(Hash256::ZERO, vec![make_tx(1, 200, 1)]);
        let hash_a = cache_tx_set(&driver, tx_set_a);

        // Set B: 3 ops
        let tx_set_b = TransactionSet::new(Hash256::ZERO, vec![make_tx(2, 200, 3)]);
        let hash_b = cache_tx_set(&driver, tx_set_b);

        // A has fewer ops, so A < B
        let result = driver.compare_tx_sets(&hash_a, &hash_b, &candidates_hash);
        assert_eq!(result, std::cmp::Ordering::Less);
    }

    #[test]
    fn test_compare_tx_sets_equal_ops_higher_fee_wins() {
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        // Same ops (1), different fees
        let tx_set_a = TransactionSet::new(Hash256::ZERO, vec![make_tx(1, 300, 1)]);
        let hash_a = cache_tx_set(&driver, tx_set_a);

        let tx_set_b = TransactionSet::new(Hash256::ZERO, vec![make_tx(2, 100, 1)]);
        let hash_b = cache_tx_set(&driver, tx_set_b);

        // For legacy tx sets (no generalized), inclusion fee == full fee.
        // A has higher fee → A > B
        let result = driver.compare_tx_sets(&hash_a, &hash_b, &candidates_hash);
        assert_eq!(result, std::cmp::Ordering::Greater);
    }

    #[test]
    fn test_compare_tx_sets_inclusion_fee_precedes_full_fee() {
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        // Both sets have equal ops (1), but different inclusion/full-fee order:
        // - A: full fee 1000, base_fee 100 -> inclusion fee 100
        // - B: full fee 400,  base_fee 400 -> inclusion fee 400
        // Criterion 2 (inclusion fee) must win over criterion 3 (full fee), so B > A.
        let tx_a = make_tx(20, 1_000, 1);
        let tx_b = make_tx(21, 400, 1);

        let tx_set_a = make_generalized_tx_set(tx_a, 100);
        let tx_set_b = make_generalized_tx_set(tx_b, 400);

        let hash_a = cache_tx_set(&driver, tx_set_a);
        let hash_b = cache_tx_set(&driver, tx_set_b);

        let result = driver.compare_tx_sets(&hash_a, &hash_b, &candidates_hash);
        assert_eq!(
            result,
            std::cmp::Ordering::Less,
            "higher inclusion fees must outrank higher full fees"
        );
    }

    #[test]
    fn test_compare_tx_sets_smaller_encoded_size_wins_when_tied() {
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        // To test criterion 4 (encoded size), we need sets with same
        // ops and fees but genuinely different sizes.
        // 1 tx with 2 ops, fee 200 -> total_ops=2, total_fee=200 (small XDR)
        // 2 txs each 1 op, fee 100 -> total_ops=2, total_fee=200 (bigger XDR)
        let tx_set_small = TransactionSet::new(Hash256::ZERO, vec![make_tx(10, 200, 2)]);
        let hash_small = cache_tx_set(&driver, tx_set_small);

        let tx_set_big = TransactionSet::new(
            Hash256::ZERO,
            vec![make_tx(11, 100, 1), make_tx(12, 100, 1)],
        );
        let hash_big = cache_tx_set(&driver, tx_set_big);

        // Both have ops=2, total_fee=200, inclusion_fee=200.
        // Smaller encoded size wins (criterion 4).
        let result = driver.compare_tx_sets(&hash_small, &hash_big, &candidates_hash);
        assert_eq!(
            result,
            std::cmp::Ordering::Greater,
            "Smaller encoded size should win when ops and fees are equal"
        );
    }

    #[test]
    fn test_compare_tx_sets_xor_tiebreak() {
        let driver = make_driver();
        // Use a known candidates_hash for deterministic tiebreak
        let candidates_hash = [0xAA; 32];

        // Two empty tx sets with different hashes
        let tx_set_a = TransactionSet::new(Hash256::from_bytes([1u8; 32]), vec![]);
        let hash_a = cache_tx_set(&driver, tx_set_a);

        let tx_set_b = TransactionSet::new(Hash256::from_bytes([2u8; 32]), vec![]);
        let hash_b = cache_tx_set(&driver, tx_set_b);

        // Both have 0 ops, 0 fees, 0 size → goes to XOR tiebreak
        let result = driver.compare_tx_sets(&hash_a, &hash_b, &candidates_hash);
        // The result should be deterministic based on hash XOR
        let a_xored = ScpDriver::xor_hash(&hash_a.0, &candidates_hash);
        let b_xored = ScpDriver::xor_hash(&hash_b.0, &candidates_hash);
        assert_eq!(result, a_xored.cmp(&b_xored));
    }

    #[test]
    fn test_compare_tx_sets_missing_set_falls_through_to_xor() {
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        // Only cache set A, leave B uncached
        let tx_set_a = TransactionSet::new(Hash256::ZERO, vec![make_tx(1, 300, 3)]);
        let hash_a = cache_tx_set(&driver, tx_set_a);

        // B is not cached (just a made-up hash)
        let hash_b = Hash256::from_bytes([99u8; 32]);

        // When one or both sets are missing from cache, all 4 criteria are skipped
        // and comparison goes straight to XOR tiebreak (criterion 5)
        let result = driver.compare_tx_sets(&hash_a, &hash_b, &candidates_hash);
        let a_xored = ScpDriver::xor_hash(&hash_a.0, &candidates_hash);
        let b_xored = ScpDriver::xor_hash(&hash_b.0, &candidates_hash);
        assert_eq!(result, a_xored.cmp(&b_xored));
    }

    #[test]
    fn test_compare_tx_sets_equal_sets_returns_equal_xor() {
        let driver = make_driver();
        let candidates_hash = [0u8; 32];

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![make_tx(1, 100, 1)]);
        let hash = cache_tx_set(&driver, tx_set);

        // Comparing a set with itself should return Equal
        let result = driver.compare_tx_sets(&hash, &hash, &candidates_hash);
        assert_eq!(result, std::cmp::Ordering::Equal);
    }
}
