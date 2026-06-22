//! Pending SCP envelope management.
//!
//! This module handles buffering and releasing SCP envelopes that arrive
//! for slots that are not yet active. Once a slot becomes active (the node
//! has caught up to that point), the pending envelopes are released for
//! processing.
//!
//! # Overview
//!
//! When a node is catching up or when SCP messages arrive for future ledger slots,
//! those envelopes cannot be processed immediately. The [`PendingEnvelopes`] manager
//! buffers these envelopes and releases them when the appropriate slot becomes active.
//!
//! # Key Features
//!
//! - **Deduplication**: Tracks envelope hashes to prevent duplicate processing
//! - **Slot Limits**: Limits the number of distinct slots buffered (matching
//!   `LEDGER_VALIDITY_BRACKET`)
//! - **Expiration**: Automatically evicts old envelopes based on configurable age limits
//!
//! # Parity with stellar-core
//!
//! stellar-core's `PendingEnvelopes` does NOT impose per-slot envelope count
//! limits. Cleanup is done by `stopAllOutsideRange(min, max, slotToKeep)`,
//! which removes entire slots outside the active window.
//!
//! Henyey adds a defense-in-depth safety cap ([`MAX_ENVELOPES_PER_SLOT`] =
//! 5000) that bounds per-slot growth. This cap is set well above the
//! theoretical honest maximum (~1500 from 50 quorum nodes × 30 messages) and
//! should never trigger during normal SCP consensus. It exists to prevent
//! memory exhaustion if a quorum member's key is compromised or a quorum-less
//! watcher is flooded. See the constant's doc comment for full derivation.

use dashmap::DashMap;
use henyey_common::Hash256;
use henyey_scp::SlotIndex;
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use stellar_xdr::ScpEnvelope;

/// Per-slot envelope safety cap. Henyey-specific defense-in-depth.
///
/// Boundary: the 5000th envelope is accepted; the 5001st is rejected with
/// [`PendingResult::PerSlotFull`].
///
/// Derivation: Stellar mainnet transitive quorum ~50 nodes. Worst-case honest
/// messages per node per slot: ~30 (multiple NOMINATE rounds + PREPARE +
/// CONFIRM + EXTERNALIZE). Theoretical honest max: 1500. Cap provides 3.3×
/// margin. Cannot be reached without a compromised quorum member key or a
/// quorum-less watcher under flood attack.
///
/// This is NOT the same as the cap removed in #1899. That cap was 100 —
/// routinely hit during normal 30-validator nomination rounds. This cap of
/// 5000 is 50× higher and requires adversarial conditions to trigger.
///
/// Not present in stellar-core. See `PARITY_STATUS.md`.
pub(crate) const MAX_ENVELOPES_PER_SLOT: usize = 5000;

/// Configuration for pending envelope management.
///
/// Parity reference — stellar-core has two independent sizing concerns:
/// - `MAX_SLOTS_TO_REMEMBER` (12) for already-externalized-slot retention
///   (`stellar-core/src/herder/Herder.h`), controlled by
///   `HerderConfig::max_externalized_slots` in henyey.
/// - `LEDGER_VALIDITY_BRACKET` (100) for envelope-acceptance horizon
///   (`stellar-core/src/herder/Herder.cpp:19`), enforced at the pre-filter
///   layer in henyey (`herder.rs::pre_filter_scp_envelope`).
///
/// Stellar-core's `PendingEnvelopes` does NOT impose a separate
/// per-slot-distance horizon inside the buffer — envelope acceptance is
/// gated ONCE at the pre-filter. Henyey previously had an additional narrow
/// gate (`max_slot_distance = 12`) here that caused issue #1807: in
/// accelerated mode (1 s/ledger) the primary could run > 12 slots ahead of
/// the captive-core observer post-catchup, and otherwise-valid EXTERNALIZE
/// envelopes were silently dropped as `SlotTooFar`. The field has been
/// removed to match stellar-core.
///
/// stellar-core also does NOT impose per-slot envelope count limits.
/// The `max_per_slot` field that previously existed here was
/// henyey-specific and caused issue #1899: during heavy-TX bursts the
/// per-slot cap filled up and critical CONFIRM/EXTERNALIZE votes were
/// dropped, stalling the node. It has been removed to match stellar-core.
///
/// However, henyey now adds a high safety cap ([`MAX_ENVELOPES_PER_SLOT`])
/// as defense-in-depth against memory exhaustion from compromised validators
/// or flood attacks on quorum-less watchers. This cap is 50× above the old
/// one and cannot be reached during honest SCP consensus.
#[derive(Debug, Clone)]
pub struct PendingConfig {
    /// Maximum number of slots to buffer.
    pub max_slots: usize,
    /// Maximum age of pending envelopes before eviction.
    pub max_age: Duration,
    /// Per-slot envelope safety cap (defense-in-depth, henyey-specific).
    /// Defaults to [`MAX_ENVELOPES_PER_SLOT`]. Tests may override with a
    /// lower value for easier verification.
    pub(crate) max_envelopes_per_slot: usize,
}

impl Default for PendingConfig {
    fn default() -> Self {
        Self {
            // Sized to cover a full `LEDGER_VALIDITY_BRACKET` window so
            // that post-catchup buffering can hold envelopes across the
            // pre-filter horizon. Bound to the constant so the invariant
            // ("buffer capacity == pre-filter horizon") is locked in at
            // the type level.
            max_slots: crate::sync_recovery::LEDGER_VALIDITY_BRACKET as usize,
            max_age: Duration::from_secs(300),
            max_envelopes_per_slot: MAX_ENVELOPES_PER_SLOT,
        }
    }
}

/// A pending SCP envelope with metadata.
///
/// Fields are private to enforce the invariant that `hash` is always
/// `Hash256::hash_xdr(&envelope)`. Use accessor methods to read fields.
#[derive(Debug, Clone)]
struct PendingEnvelope {
    /// The SCP envelope.
    envelope: ScpEnvelope,
    /// When this envelope was received.
    received_at: Instant,
    /// Hash of the envelope for deduplication.
    hash: Hash256,
}

impl PendingEnvelope {
    /// Create a new pending envelope.
    fn new(envelope: ScpEnvelope) -> Self {
        // Compute envelope hash for deduplication
        let hash = Hash256::hash_xdr(&envelope);
        Self {
            envelope,
            received_at: Instant::now(),
            hash,
        }
    }

    /// Returns the precomputed hash of this envelope.
    fn hash(&self) -> Hash256 {
        self.hash
    }

    /// Consumes the pending envelope, returning the inner SCP envelope.
    fn into_envelope(self) -> ScpEnvelope {
        self.envelope
    }

    /// Check if this envelope has expired.
    fn is_expired(&self, max_age: Duration) -> bool {
        self.received_at.elapsed() > max_age
    }
}

/// Result of adding an envelope to pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingResult {
    /// Envelope was added successfully.
    Added,
    /// Envelope is a duplicate.
    Duplicate,
    /// Slot is too old.
    SlotTooOld,
    /// Buffer is full (slot-count limit).
    BufferFull,
    /// Per-slot safety cap reached (defense-in-depth, henyey-specific).
    /// The slot already has [`MAX_ENVELOPES_PER_SLOT`] buffered envelopes.
    PerSlotFull,
}

/// Manages pending SCP envelopes for future slots.
///
/// When the node is catching up or when envelopes arrive for future slots,
/// they are buffered here until the slot becomes active.
///
/// # Concurrency
///
/// The `slots` field uses `RwLock<BTreeMap>` (not `DashMap`) because:
/// - All operations scan or mutate the entire map (no shard-level benefit).
/// - The collection is small (max 100 entries).
/// - A single lock eliminates TOCTOU races on capacity checks.
///
/// The `seen_hashes` field uses `DashMap` because it is high-cardinality
/// (up to 500K entries) and accessed via independent point lookups.
pub struct PendingEnvelopes {
    /// Configuration.
    config: PendingConfig,
    /// Pending envelopes organized by slot. BTreeMap for ordered iteration
    /// matching stellar-core's `std::map` semantics.
    slots: RwLock<BTreeMap<SlotIndex, Vec<PendingEnvelope>>>,
    /// Seen envelope hashes for deduplication.
    seen_hashes: DashMap<Hash256, ()>,
    /// Current active slot.
    current_slot: RwLock<SlotIndex>,
    /// Statistics.
    stats: RwLock<PendingStats>,
    /// Last slot for which a BufferFull warning was emitted (rate-limiting).
    last_buffer_full_warn_slot: AtomicU64,
    /// Last slot for which a PerSlotFull warning was emitted (rate-limiting).
    last_per_slot_full_warn_slot: AtomicU64,
}

/// Statistics about pending envelope management.
#[derive(Debug, Clone, Default)]
pub struct PendingStats {
    /// Total envelopes received.
    pub received: u64,
    /// Envelopes added to pending.
    pub added: u64,
    /// Duplicate envelopes rejected.
    pub duplicates: u64,
    /// Envelopes rejected for being too old.
    pub too_old: u64,
    /// Envelopes released for processing.
    pub released: u64,
    /// Envelopes evicted due to expiration.
    pub evicted: u64,
    /// Total BufferFull rejections (slot-count limit).
    pub buffer_full: u64,
    /// Per-slot safety cap rejections (defense-in-depth).
    pub per_slot_full: u64,
    /// High-water mark: largest envelope count observed in any single slot.
    pub max_envelopes_per_slot: u64,
}

impl PendingEnvelopes {
    /// Create a new pending envelope manager.
    pub fn new(config: PendingConfig) -> Self {
        Self {
            config,
            slots: RwLock::new(BTreeMap::new()),
            seen_hashes: DashMap::new(),
            current_slot: RwLock::new(0),
            stats: RwLock::new(PendingStats::default()),
            last_buffer_full_warn_slot: AtomicU64::new(0),
            last_per_slot_full_warn_slot: AtomicU64::new(0),
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(PendingConfig::default())
    }

    /// Set the current active slot.
    pub fn set_current_slot(&self, slot: SlotIndex) {
        let mut current = self.current_slot.write();
        *current = slot;
    }

    /// Get the current active slot.
    pub fn current_slot(&self) -> SlotIndex {
        *self.current_slot.read()
    }

    /// Add an envelope for a future slot.
    pub fn add(&self, slot: SlotIndex, envelope: ScpEnvelope) -> PendingResult {
        self.stats.write().received += 1;

        let current = self.current_slot();

        // Check if slot is too old
        if slot < current {
            self.stats.write().too_old += 1;
            return PendingResult::SlotTooOld;
        }

        // No per-slot-distance horizon here — the envelope-acceptance horizon
        // (`LEDGER_VALIDITY_BRACKET = 100`) is enforced at the pre-filter
        // layer in `herder::pre_filter_scp_envelope`, matching stellar-core's
        // single-gate design. See `PendingConfig` doc comment and #1807.

        let pending = PendingEnvelope::new(envelope);

        // Atomically check-and-insert to prevent TOCTOU race on duplicates.
        // Using entry() ensures that concurrent submissions of the same hash
        // cannot both pass the duplicate check.
        match self.seen_hashes.entry(pending.hash()) {
            dashmap::mapref::entry::Entry::Occupied(_) => {
                self.stats.write().duplicates += 1;
                return PendingResult::Duplicate;
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(());
            }
        }
        // Entry guard is dropped here — no shard lock held past this point.

        // Take the slots write lock for the remainder of this method.
        // This eliminates the TOCTOU race between capacity check and insertion
        // that existed with the previous DashMap approach.
        let mut slots = self.slots.write();

        // Existing slot — inline-evict expired, check per-slot cap, then append.
        if let Some(existing) = slots.get_mut(&slot) {
            // Inline eviction: remove expired entries from this specific slot
            // before checking cap. Prevents stale entries from consuming budget.
            let max_age = self.config.max_age;
            let expired_hashes: Vec<Hash256> = existing
                .iter()
                .filter(|e| e.is_expired(max_age))
                .map(|e| e.hash())
                .collect();
            if !expired_hashes.is_empty() {
                existing.retain(|e| !e.is_expired(max_age));
                for h in &expired_hashes {
                    self.seen_hashes.remove(h);
                }
                self.stats.write().evicted += expired_hashes.len() as u64;
            }

            // Per-slot safety cap (defense-in-depth).
            if self.config.max_envelopes_per_slot > 0
                && existing.len() >= self.config.max_envelopes_per_slot
            {
                // Clean up the eagerly-claimed hash since envelope is rejected.
                self.seen_hashes.remove(&pending.hash());
                self.stats.write().per_slot_full += 1;
                return PendingResult::PerSlotFull;
            }
            existing.push(pending);
            let count = existing.len() as u64;
            let mut stats = self.stats.write();
            stats.added += 1;
            if count > stats.max_envelopes_per_slot {
                stats.max_envelopes_per_slot = count;
            }
            return PendingResult::Added;
        }

        // New slot — enforce max_slots with eviction.
        // Capacity check and insertion are under the same write lock — no TOCTOU.
        if slots.len() >= self.config.max_slots {
            Self::evict_old_slots_inner(&mut slots, current, &self.seen_hashes, &self.stats);
            if slots.len() >= self.config.max_slots {
                // Clean up the eagerly-claimed hash since envelope is rejected.
                self.seen_hashes.remove(&pending.hash());
                self.stats.write().buffer_full += 1;
                return PendingResult::BufferFull;
            }
        }

        // Insert into new slot.
        slots.entry(slot).or_default().push(pending);

        let mut stats = self.stats.write();
        stats.added += 1;
        if stats.max_envelopes_per_slot == 0 {
            stats.max_envelopes_per_slot = 1;
        }
        PendingResult::Added
    }

    /// Release all envelopes for a slot that has become active.
    ///
    /// Returns envelopes in LIFO order (last-added first), matching
    /// stellar-core's `PendingEnvelopes::pop()` which uses `v.back()` /
    /// `pop_back()`.
    #[cfg(test)]
    fn release(&self, slot: SlotIndex) -> Vec<ScpEnvelope> {
        let envelopes = {
            let mut slots = self.slots.write();
            slots.remove(&slot)
        };

        if let Some(envelopes) = envelopes {
            let count = envelopes.len() as u64;
            self.stats.write().released += count;

            // Remove from seen hashes
            for env in &envelopes {
                self.seen_hashes.remove(&env.hash());
            }

            envelopes
                .into_iter()
                .rev()
                .filter(|e| !e.is_expired(self.config.max_age))
                .map(|e| e.into_envelope())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Release all envelopes up to and including the given slot.
    ///
    /// Slots are returned in ascending order (via `BTreeMap`). Within each
    /// slot, envelopes are in LIFO order (last-added first), matching
    /// stellar-core's `PendingEnvelopes::pop()` semantics.
    pub fn release_up_to(&self, slot: SlotIndex) -> BTreeMap<SlotIndex, Vec<ScpEnvelope>> {
        // Collect all slots to release under one lock acquisition.
        let removed: Vec<(SlotIndex, Vec<PendingEnvelope>)> = {
            let mut slots = self.slots.write();
            let keys: Vec<SlotIndex> = slots.keys().filter(|k| **k <= slot).copied().collect();
            keys.into_iter()
                .filter_map(|s| slots.remove(&s).map(|v| (s, v)))
                .collect()
        };

        let mut result = BTreeMap::new();
        let max_age = self.config.max_age;

        for (s, envelopes) in removed {
            let count = envelopes.len() as u64;
            self.stats.write().released += count;

            for env in &envelopes {
                self.seen_hashes.remove(&env.hash());
            }

            let filtered: Vec<ScpEnvelope> = envelopes
                .into_iter()
                .rev()
                .filter(|e| !e.is_expired(max_age))
                .map(|e| e.into_envelope())
                .collect();

            if !filtered.is_empty() {
                result.insert(s, filtered);
            }
        }

        result
    }

    /// Evict old slots that are behind the current slot.
    ///
    /// This is a helper called from within `add()` which already holds the
    /// slots write lock. Takes a mutable reference to the map directly.
    fn evict_old_slots_inner(
        slots: &mut BTreeMap<SlotIndex, Vec<PendingEnvelope>>,
        current: SlotIndex,
        seen_hashes: &DashMap<Hash256, ()>,
        stats: &RwLock<PendingStats>,
    ) {
        let old_slots: Vec<SlotIndex> = slots.keys().filter(|k| **k < current).copied().collect();

        let mut total_evicted = 0u64;
        for slot in old_slots {
            if let Some(envelopes) = slots.remove(&slot) {
                total_evicted += envelopes.len() as u64;
                for env in envelopes {
                    seen_hashes.remove(&env.hash());
                }
            }
        }
        if total_evicted > 0 {
            stats.write().evicted += total_evicted;
        }
    }

    /// Evict expired envelopes from all slots.
    pub fn evict_expired(&self) {
        let max_age = self.config.max_age;
        let mut expired_hashes = Vec::new();
        let mut total_removed = 0u64;

        {
            let mut slots = self.slots.write();

            for envelopes in slots.values_mut() {
                let initial_len = envelopes.len();

                // Collect hashes of expired envelopes
                for e in envelopes.iter() {
                    if e.is_expired(max_age) {
                        expired_hashes.push(e.hash());
                    }
                }

                // Remove expired envelopes
                envelopes.retain(|e| !e.is_expired(max_age));

                let removed = initial_len - envelopes.len();
                total_removed += removed as u64;
            }

            // Remove empty slots
            slots.retain(|_, v| !v.is_empty());
        }

        // Cleanup outside the lock
        if total_removed > 0 {
            self.stats.write().evicted += total_removed;
            for hash in expired_hashes {
                self.seen_hashes.remove(&hash);
            }
        }
    }

    /// Get the number of pending envelopes.
    pub fn len(&self) -> usize {
        self.slots.read().values().map(|v| v.len()).sum()
    }

    /// Check if there are no pending envelopes.
    pub fn is_empty(&self) -> bool {
        self.slots.read().values().all(|v| v.is_empty())
    }

    /// Get the number of slots with pending envelopes.
    pub fn slot_count(&self) -> usize {
        self.slots.read().len()
    }

    /// Get statistics.
    pub fn stats(&self) -> PendingStats {
        self.stats.read().clone()
    }

    /// Check if there are pending envelopes for a slot.
    pub fn has_pending(&self, slot: SlotIndex) -> bool {
        self.slots
            .read()
            .get(&slot)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    /// Get the count of pending envelopes for a slot.
    pub fn pending_count(&self, slot: SlotIndex) -> usize {
        self.slots.read().get(&slot).map(|v| v.len()).unwrap_or(0)
    }

    /// Clear all pending envelopes.
    pub fn clear(&self) {
        let (slots_count, seen_count) = {
            let mut slots = self.slots.write();
            let sc = slots.len();
            slots.clear();
            (sc, self.seen_hashes.len())
        };
        self.seen_hashes.clear();
        if slots_count > 0 || seen_count > 0 {
            tracing::info!(slots_count, seen_count, "Cleared pending_envelopes");
        }
    }

    /// Remove all buffered slots below `min_slot`, cleaning up seen_hashes.
    ///
    /// Matches stellar-core's `stopAllOutsideRange` lower bound — removes
    /// stale slots that can no longer be processed. Does NOT impose an upper
    /// bound, matching stellar-core behavior where future-slot bounds are
    /// unreliable when not tracking.
    pub fn purge_slots_below(&self, min_slot: SlotIndex) {
        let removed: Vec<(SlotIndex, Vec<PendingEnvelope>)> = {
            let mut slots = self.slots.write();
            let keys: Vec<SlotIndex> = slots.keys().filter(|k| **k < min_slot).copied().collect();
            keys.into_iter()
                .filter_map(|s| slots.remove(&s).map(|v| (s, v)))
                .collect()
        };

        let mut total_evicted = 0u64;
        for (_, envelopes) in removed {
            total_evicted += envelopes.len() as u64;
            for env in envelopes {
                self.seen_hashes.remove(&env.hash());
            }
        }
        if total_evicted > 0 {
            self.stats.write().evicted += total_evicted;
            tracing::debug!(
                min_slot,
                total_evicted,
                "Purged pending slots below threshold"
            );
        }
    }

    /// Returns the last slot for which a BufferFull warning was logged.
    /// Used for rate-limiting warnings (once per slot).
    pub fn last_buffer_full_warn_slot(&self) -> u64 {
        self.last_buffer_full_warn_slot.load(Ordering::Relaxed)
    }

    /// Set the last slot for which a BufferFull warning was logged.
    pub fn set_last_buffer_full_warn_slot(&self, slot: u64) {
        self.last_buffer_full_warn_slot
            .store(slot, Ordering::Relaxed);
    }

    /// Returns the last slot for which a PerSlotFull warning was logged.
    pub fn last_per_slot_full_warn_slot(&self) -> u64 {
        self.last_per_slot_full_warn_slot.load(Ordering::Relaxed)
    }

    /// Set the last slot for which a PerSlotFull warning was logged.
    pub fn set_last_per_slot_full_warn_slot(&self, slot: u64) {
        self.last_per_slot_full_warn_slot
            .store(slot, Ordering::Relaxed);
    }
}

impl Default for PendingEnvelopes {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::{
        Hash, NodeId as XdrNodeId, PublicKey, ScpEnvelope, ScpNomination, ScpStatement,
        ScpStatementPledges, Uint256,
    };

    fn make_test_envelope(slot: SlotIndex) -> ScpEnvelope {
        // Create a minimal SCP envelope for testing
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32])));

        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash([0u8; 32]),
                    votes: vec![].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: stellar_xdr::Signature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    #[test]
    fn test_add_and_release() {
        let pending = PendingEnvelopes::with_defaults();
        pending.set_current_slot(100);

        // Add envelope for slot 101
        let envelope = make_test_envelope(101);
        let result = pending.add(101, envelope.clone());
        assert_eq!(result, PendingResult::Added);
        assert_eq!(pending.len(), 1);

        // Release slot 101
        let released = pending.release(101);
        assert_eq!(released.len(), 1);
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn test_duplicate_detection() {
        let pending = PendingEnvelopes::with_defaults();
        pending.set_current_slot(100);

        let envelope = make_test_envelope(101);

        let result1 = pending.add(101, envelope.clone());
        assert_eq!(result1, PendingResult::Added);

        let result2 = pending.add(101, envelope);
        assert_eq!(result2, PendingResult::Duplicate);

        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn test_slot_too_old() {
        let pending = PendingEnvelopes::with_defaults();
        pending.set_current_slot(100);

        let envelope = make_test_envelope(99);
        let result = pending.add(99, envelope);
        assert_eq!(result, PendingResult::SlotTooOld);
    }

    /// Regression for issue #1807.
    ///
    /// Stellar-core's `PendingEnvelopes` (`stellar-core/src/herder/
    /// PendingEnvelopes.cpp::recvSCPEnvelope`) has no per-slot-distance cap;
    /// envelope acceptance is enforced exactly once at the pre-filter layer
    /// (`HerderImpl::recvSCPEnvelope` via `LEDGER_VALIDITY_BRACKET = 100`).
    /// Henyey previously imposed a narrower second gate here that dropped
    /// EXTERNALIZE envelopes the primary sent for slots > 12 ahead of the
    /// post-catchup observer's `tracking_slot` — freezing captive-core in
    /// the accelerated-mode Quickstart shards. The field and gate are now
    /// removed; any slot within the pre-filter horizon must be buffered.
    #[test]
    fn test_far_future_slot_is_buffered_not_rejected() {
        let pending = PendingEnvelopes::with_defaults();
        pending.set_current_slot(272);

        // Slot 320 is 48 ahead of current_slot — well within the pre-filter
        // horizon of `LEDGER_VALIDITY_BRACKET = 100`. Previously this was
        // rejected as `SlotTooFar`; now it must be buffered.
        let envelope = make_test_envelope(320);
        assert_eq!(pending.add(320, envelope), PendingResult::Added);
    }

    #[test]
    fn test_release_up_to() {
        let pending = PendingEnvelopes::with_defaults();
        pending.set_current_slot(100);

        pending.add(101, make_test_envelope(101));
        pending.add(102, make_test_envelope(102));
        pending.add(103, make_test_envelope(103));

        assert_eq!(pending.slot_count(), 3);

        let released = pending.release_up_to(102);
        assert_eq!(released.len(), 2);
        assert!(released.contains_key(&101));
        assert!(released.contains_key(&102));
        assert_eq!(pending.slot_count(), 1);
    }

    fn make_test_envelope_with_node(slot: SlotIndex, node_seed: u8) -> ScpEnvelope {
        let node_id = XdrNodeId(PublicKey::PublicKeyTypeEd25519(Uint256([node_seed; 32])));
        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash([0u8; 32]),
                    votes: vec![].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: stellar_xdr::Signature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    /// [AUDIT-XH7] When the buffer is full (slot-count limit), the envelope's
    /// hash must not remain in seen_hashes. Otherwise the envelope is
    /// permanently rejected as a "duplicate" even though it was never actually
    /// buffered.
    ///
    /// Updated for #1899: the per-slot cap no longer exists; BufferFull is
    /// now triggered only by the slot-count limit.
    #[test]
    fn test_audit_xh7_buffer_full_does_not_poison_seen_hashes() {
        let config = PendingConfig {
            max_slots: 1, // Only allow 1 slot
            ..Default::default()
        };
        let pending = PendingEnvelopes::new(config);
        pending.set_current_slot(100);

        // Fill the single slot
        let env1 = make_test_envelope_with_node(101, 1);
        assert_eq!(pending.add(101, env1), PendingResult::Added);

        // Try to add to a different slot — should be BufferFull (slot-count limit)
        let env2 = make_test_envelope_with_node(102, 2);
        assert_eq!(pending.add(102, env2.clone()), PendingResult::BufferFull);

        // Release slot 101 to free space
        pending.release(101);

        // Now re-add the envelope that was previously rejected as BufferFull.
        // Before fix: returns Duplicate (ghost entry in seen_hashes)
        // After fix: returns Added
        let result = pending.add(102, env2);
        assert_eq!(
            result,
            PendingResult::Added,
            "Envelope rejected as BufferFull should not be permanently stuck in seen_hashes"
        );
    }

    /// Verify release_up_to drains all intermediate slots in ascending order.
    /// This is the core invariant that advance_tracking_slot relies on
    /// for parity with stellar-core's processSCPQueueUpToIndex.
    #[test]
    fn test_release_up_to_intermediate_slots() {
        let pending = PendingEnvelopes::with_defaults();
        pending.set_current_slot(100);

        // Buffer envelopes for 5 consecutive slots
        for slot in 101..=105 {
            pending.add(slot, make_test_envelope(slot));
        }
        assert_eq!(pending.slot_count(), 5);

        // Simulate fast-forward: release up to slot 104
        let released = pending.release_up_to(104);

        // All 4 intermediate slots (101-104) must be released
        assert_eq!(released.len(), 4);
        let released_slots: Vec<SlotIndex> = released.keys().copied().collect();
        assert_eq!(released_slots, vec![101, 102, 103, 104]);

        // Each slot should have exactly 1 envelope
        for (_slot, envs) in &released {
            assert_eq!(envs.len(), 1);
        }

        // Slot 105 should remain in the buffer
        assert_eq!(pending.slot_count(), 1);
    }

    /// Regression for #1899, Test A: 200+ envelopes with distinct node IDs
    /// in one slot should all be accepted. The safety cap is 5000, far above
    /// the 201 tested here. The old `max_per_slot = 100` was removed in #1899;
    /// the new safety cap (`MAX_ENVELOPES_PER_SLOT = 5000`) added in #2408 is
    /// high enough to never trigger during honest SCP operation.
    #[test]
    fn test_issue_1899_no_per_slot_cap() {
        let pending = PendingEnvelopes::with_defaults();
        pending.set_current_slot(100);

        for i in 0..=200u8 {
            let env = make_test_envelope_with_node(101, i);
            assert_eq!(
                pending.add(101, env),
                PendingResult::Added,
                "Envelope from node {i} should be accepted — no per-slot cap"
            );
        }
        assert_eq!(pending.pending_count(101), 201);
        let stats = pending.stats();
        assert_eq!(stats.max_envelopes_per_slot, 201);
    }

    /// Regression for #1899, Test B: Append to existing slot when max_slots
    /// is full — must be accepted without eviction.
    #[test]
    fn test_issue_1899_existing_slot_bypasses_max_slots_check() {
        let config = PendingConfig {
            max_slots: 2,
            ..Default::default()
        };
        let pending = PendingEnvelopes::new(config);
        pending.set_current_slot(100);

        // Fill both slots
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 1)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(102, make_test_envelope_with_node(102, 2)),
            PendingResult::Added
        );
        assert_eq!(pending.slot_count(), 2);

        // Adding to an existing slot should succeed even though max_slots is full
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 3)),
            PendingResult::Added,
            "Appending to existing slot must succeed regardless of slot-count limit"
        );
        assert_eq!(pending.pending_count(101), 2);

        // Adding a new third slot should fail
        assert_eq!(
            pending.add(103, make_test_envelope_with_node(103, 4)),
            PendingResult::BufferFull,
            "New slot should be rejected when max_slots is full"
        );
    }

    /// Regression for #1899, Test C: Slot-count eviction with small max_slots.
    #[test]
    fn test_issue_1899_slot_count_eviction() {
        let config = PendingConfig {
            max_slots: 2,
            ..Default::default()
        };
        let pending = PendingEnvelopes::new(config);
        pending.set_current_slot(100);

        // Fill both slots
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 1)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(102, make_test_envelope_with_node(102, 2)),
            PendingResult::Added
        );

        // Advance current_slot so slot 101 becomes old
        pending.set_current_slot(102);

        // Now adding slot 103 should evict slot 101 and succeed
        assert_eq!(
            pending.add(103, make_test_envelope_with_node(103, 3)),
            PendingResult::Added,
            "Should evict old slot 101 and accept new slot 103"
        );
        assert_eq!(pending.slot_count(), 2);
        assert!(
            !pending.has_pending(101),
            "Slot 101 should have been evicted"
        );
    }

    /// Regression for #1899, Test D: Non-tracking mode (current_slot=0)
    /// should still enforce slot-count limit.
    #[test]
    fn test_issue_1899_non_tracking_buffer_full() {
        let config = PendingConfig {
            max_slots: 2,
            ..Default::default()
        };
        let pending = PendingEnvelopes::new(config);
        // current_slot = 0 (default, non-tracking)

        assert_eq!(
            pending.add(100, make_test_envelope_with_node(100, 1)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 2)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(102, make_test_envelope_with_node(102, 3)),
            PendingResult::BufferFull,
            "Non-tracking mode: no old slots to evict, must return BufferFull"
        );
    }

    /// Regression for #1899, Test E: seen_hashes are cleaned up after eviction.
    #[test]
    fn test_issue_1899_seen_hashes_cleaned_on_eviction() {
        let config = PendingConfig {
            max_slots: 3,
            ..Default::default()
        };
        let pending = PendingEnvelopes::new(config);
        pending.set_current_slot(100);

        let env = make_test_envelope_with_node(101, 1);
        assert_eq!(pending.add(101, env.clone()), PendingResult::Added);
        assert_eq!(
            pending.add(102, make_test_envelope_with_node(102, 2)),
            PendingResult::Added
        );

        // Advance and evict slot 101 via evict_old_slots (triggered by slot-count pressure)
        pending.set_current_slot(102);
        // Add slot 103 — this fills to max_slots=3 (102, 103, and maybe evicts 101)
        assert_eq!(
            pending.add(103, make_test_envelope_with_node(103, 3)),
            PendingResult::Added
        );
        // Manually purge below 102 to evict slot 101
        pending.purge_slots_below(102);

        // Slot 101 was evicted. Re-add the same envelope — should not be Duplicate.
        pending.set_current_slot(100); // make slot eligible again
        let result = pending.add(101, env);
        assert_eq!(
            result,
            PendingResult::Added,
            "After eviction, envelope hash must be removed from seen_hashes"
        );
    }

    /// Regression for #1899, Test G: Observability counters are correct.
    #[test]
    fn test_issue_1899_observability_counters() {
        let config = PendingConfig {
            max_slots: 1,
            ..Default::default()
        };
        let pending = PendingEnvelopes::new(config);
        pending.set_current_slot(100);

        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 1)),
            PendingResult::Added
        );
        // This triggers BufferFull (slot-count limit)
        assert_eq!(
            pending.add(102, make_test_envelope_with_node(102, 2)),
            PendingResult::BufferFull
        );
        assert_eq!(
            pending.add(103, make_test_envelope_with_node(103, 3)),
            PendingResult::BufferFull
        );

        let stats = pending.stats();
        assert_eq!(stats.buffer_full, 2, "Two BufferFull rejections");
        assert_eq!(
            stats.max_envelopes_per_slot, 1,
            "High-water mark for single envelope"
        );
        assert_eq!(stats.added, 1);
        assert_eq!(stats.received, 3);
    }

    /// Regression for #1899, Test H: purge_slots_below cleans seen_hashes.
    #[test]
    fn test_issue_1899_purge_slots_below_cleans_seen_hashes() {
        let pending = PendingEnvelopes::with_defaults();
        pending.set_current_slot(100);

        let env1 = make_test_envelope_with_node(101, 1);
        let env2 = make_test_envelope_with_node(102, 2);
        let env3 = make_test_envelope_with_node(103, 3);
        assert_eq!(pending.add(101, env1.clone()), PendingResult::Added);
        assert_eq!(pending.add(102, env2.clone()), PendingResult::Added);
        assert_eq!(pending.add(103, env3), PendingResult::Added);
        assert_eq!(pending.slot_count(), 3);

        // Purge everything below 103
        pending.purge_slots_below(103);
        assert_eq!(pending.slot_count(), 1, "Only slot 103 should remain");
        assert!(!pending.has_pending(101));
        assert!(!pending.has_pending(102));
        assert!(pending.has_pending(103));

        // Re-add the purged envelopes — should not be Duplicate
        pending.set_current_slot(100);
        assert_eq!(
            pending.add(101, env1),
            PendingResult::Added,
            "After purge, seen_hashes for slot 101 must be cleared"
        );
        assert_eq!(
            pending.add(102, env2),
            PendingResult::Added,
            "After purge, seen_hashes for slot 102 must be cleared"
        );

        let stats = pending.stats();
        assert_eq!(stats.evicted, 2, "Two envelopes evicted by purge");
    }

    /// Helper: extract the node_seed byte from an envelope created by
    /// `make_test_envelope_with_node`.
    fn node_seed(env: &ScpEnvelope) -> u8 {
        match &env.statement.node_id.0 {
            PublicKey::PublicKeyTypeEd25519(Uint256(bytes)) => bytes[0],
        }
    }

    /// Regression for #1969: release_up_to must return envelopes in
    /// ascending slot order, with intra-slot LIFO (last-added first) to
    /// match stellar-core's PendingEnvelopes::pop() semantics.
    #[test]
    fn test_issue_1969_release_lifo_within_slot() {
        let pending = PendingEnvelopes::with_defaults();
        pending.set_current_slot(99);

        // Slot 100: add 3 envelopes with node seeds 1, 2, 3
        assert_eq!(
            pending.add(100, make_test_envelope_with_node(100, 1)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(100, make_test_envelope_with_node(100, 2)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(100, make_test_envelope_with_node(100, 3)),
            PendingResult::Added
        );

        // Slot 101: add 3 envelopes with node seeds 4, 5, 6
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 4)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 5)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 6)),
            PendingResult::Added
        );

        let released = pending.release_up_to(101);

        // Cross-slot: ascending order
        let slots: Vec<u64> = released.keys().copied().collect();
        assert_eq!(slots, vec![100, 101], "Slots must be in ascending order");

        // Intra-slot LIFO for slot 100: last-added (seed=3) first
        let slot_100: Vec<u8> = released[&100].iter().map(node_seed).collect();
        assert_eq!(
            slot_100,
            vec![3, 2, 1],
            "Slot 100 must be LIFO: last-added envelope first"
        );

        // Intra-slot LIFO for slot 101: last-added (seed=6) first
        let slot_101: Vec<u8> = released[&101].iter().map(node_seed).collect();
        assert_eq!(
            slot_101,
            vec![6, 5, 4],
            "Slot 101 must be LIFO: last-added envelope first"
        );
    }

    // --- Per-slot safety cap tests (issue #2408) ---

    /// Helper: create a PendingEnvelopes with a low per-slot cap for testing.
    fn pending_with_per_slot_cap(cap: usize) -> PendingEnvelopes {
        PendingEnvelopes::new(PendingConfig {
            max_envelopes_per_slot: cap,
            ..Default::default()
        })
    }

    /// The per-slot safety cap is enforced: after `cap` envelopes, the next
    /// returns `PerSlotFull`.
    #[test]
    fn test_per_slot_safety_cap_enforced() {
        let pending = pending_with_per_slot_cap(3);
        pending.set_current_slot(100);

        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 1)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 2)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 3)),
            PendingResult::Added
        );
        // 4th envelope hits the cap
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 4)),
            PendingResult::PerSlotFull,
            "4th envelope should be rejected by per-slot safety cap of 3"
        );
    }

    /// PerSlotFull must not poison seen_hashes — the rejected envelope can
    /// be re-added after the slot is purged and recreated.
    #[test]
    fn test_per_slot_full_does_not_poison_seen_hashes() {
        let pending = pending_with_per_slot_cap(2);
        pending.set_current_slot(100);

        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 1)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 2)),
            PendingResult::Added
        );

        // This envelope is rejected by cap
        let env3 = make_test_envelope_with_node(101, 3);
        assert_eq!(pending.add(101, env3.clone()), PendingResult::PerSlotFull);

        // Purge slot 101
        pending.purge_slots_below(102);
        pending.set_current_slot(100);

        // Re-add — should succeed (hash was not inserted into seen_hashes)
        assert_eq!(
            pending.add(101, env3),
            PendingResult::Added,
            "Envelope rejected by PerSlotFull must not be permanently stuck in seen_hashes"
        );
    }

    /// PerSlotFull on slot A does not affect slot B.
    #[test]
    fn test_per_slot_full_does_not_affect_other_slots() {
        let pending = pending_with_per_slot_cap(2);
        pending.set_current_slot(100);

        // Fill slot 101 to cap
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 1)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 2)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 3)),
            PendingResult::PerSlotFull
        );

        // Slot 102 should still accept
        assert_eq!(
            pending.add(102, make_test_envelope_with_node(102, 4)),
            PendingResult::Added,
            "PerSlotFull on slot 101 must not affect slot 102"
        );
    }

    /// Inline eviction frees space before cap check: expired entries are
    /// removed, allowing new envelopes below the cap.
    #[test]
    fn test_per_slot_full_inline_eviction_frees_space() {
        let config = PendingConfig {
            max_envelopes_per_slot: 2,
            max_age: Duration::from_millis(1), // very short expiry
            ..Default::default()
        };
        let pending = PendingEnvelopes::new(config);
        pending.set_current_slot(100);

        // Add 2 envelopes (fills to cap)
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 1)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 2)),
            PendingResult::Added
        );

        // Wait for them to expire
        std::thread::sleep(Duration::from_millis(10));

        // Next add should succeed because inline eviction removes expired entries
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 3)),
            PendingResult::Added,
            "Inline eviction should free space below cap"
        );

        // Only the new envelope remains
        assert_eq!(pending.pending_count(101), 1);
    }

    /// The per_slot_full stats counter increments on each rejection.
    #[test]
    fn test_per_slot_full_stats_counter() {
        let pending = pending_with_per_slot_cap(1);
        pending.set_current_slot(100);

        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 1)),
            PendingResult::Added
        );
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 2)),
            PendingResult::PerSlotFull
        );
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 3)),
            PendingResult::PerSlotFull
        );

        let stats = pending.stats();
        assert_eq!(stats.per_slot_full, 2, "Two per-slot-full rejections");
        assert_eq!(stats.added, 1);
        assert_eq!(stats.received, 3);
    }

    /// At cap, a duplicate envelope still returns Duplicate (not PerSlotFull).
    /// Dedup check precedes cap check in the flow.
    #[test]
    fn test_per_slot_full_duplicate_detected_at_cap() {
        let pending = pending_with_per_slot_cap(2);
        pending.set_current_slot(100);

        let env1 = make_test_envelope_with_node(101, 1);
        assert_eq!(pending.add(101, env1.clone()), PendingResult::Added);
        assert_eq!(
            pending.add(101, make_test_envelope_with_node(101, 2)),
            PendingResult::Added
        );

        // Slot is at cap. Sending a duplicate should return Duplicate, not PerSlotFull.
        assert_eq!(
            pending.add(101, env1),
            PendingResult::Duplicate,
            "Duplicate detection must work even at per-slot cap"
        );
    }

    /// The last_per_slot_full_warn_slot atomic is separate from
    /// last_buffer_full_warn_slot.
    #[test]
    fn test_per_slot_full_separate_warn_tracking() {
        let pending = pending_with_per_slot_cap(1);
        pending.set_current_slot(100);

        // Set buffer-full warn to slot 200
        pending.set_last_buffer_full_warn_slot(200);

        // Per-slot-full warn should be independent
        assert_eq!(pending.last_per_slot_full_warn_slot(), 0);
        pending.set_last_per_slot_full_warn_slot(300);
        assert_eq!(pending.last_per_slot_full_warn_slot(), 300);
        assert_eq!(
            pending.last_buffer_full_warn_slot(),
            200,
            "Buffer-full warn slot must not be affected"
        );
    }

    /// Concurrent duplicate deduplication: exactly one thread wins, rest get Duplicate.
    /// Regression test for the TOCTOU race in seen_hashes (issue #2474).
    #[test]
    fn test_concurrent_duplicate_deduplication() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const NUM_THREADS: usize = 16;
        const ITERATIONS: usize = 100;

        for _ in 0..ITERATIONS {
            let pending = Arc::new(PendingEnvelopes::with_defaults());
            pending.set_current_slot(100);

            let envelope = make_test_envelope_with_node(101, 42);
            let barrier = Arc::new(Barrier::new(NUM_THREADS));

            let handles: Vec<_> = (0..NUM_THREADS)
                .map(|_| {
                    let pending = Arc::clone(&pending);
                    let env = envelope.clone();
                    let barrier = Arc::clone(&barrier);
                    thread::spawn(move || {
                        barrier.wait();
                        pending.add(101, env)
                    })
                })
                .collect();

            let results: Vec<PendingResult> =
                handles.into_iter().map(|h| h.join().unwrap()).collect();

            let added_count = results
                .iter()
                .filter(|r| **r == PendingResult::Added)
                .count();
            let duplicate_count = results
                .iter()
                .filter(|r| **r == PendingResult::Duplicate)
                .count();

            assert_eq!(
                added_count, 1,
                "Exactly one thread should succeed with Added, got {added_count}"
            );
            assert_eq!(
                duplicate_count,
                NUM_THREADS - 1,
                "All other threads should get Duplicate, got {duplicate_count}"
            );
        }
    }

    /// When concurrent duplicates race and the claimant hits PerSlotFull,
    /// the hash must be cleaned up so the envelope is re-submittable.
    #[test]
    fn test_concurrent_dedup_with_per_slot_full_cleanup() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        // Cap at 1 envelope per slot.
        let pending = Arc::new(PendingEnvelopes::new(PendingConfig {
            max_envelopes_per_slot: 1,
            ..Default::default()
        }));
        pending.set_current_slot(100);

        // Fill slot 101 to capacity.
        let env1 = make_test_envelope_with_node(101, 1);
        assert_eq!(pending.add(101, env1), PendingResult::Added);

        // Now race N threads with a new envelope for the same (full) slot.
        const NUM_THREADS: usize = 8;
        let env2 = make_test_envelope_with_node(101, 2);
        let barrier = Arc::new(Barrier::new(NUM_THREADS));

        let handles: Vec<_> = (0..NUM_THREADS)
            .map(|_| {
                let pending = Arc::clone(&pending);
                let env = env2.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    pending.add(101, env)
                })
            })
            .collect();

        let results: Vec<PendingResult> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // One thread claims the hash and hits PerSlotFull (cleans up).
        // Others get Duplicate (the brief window where hash is claimed).
        // After all threads finish, the hash should NOT be in seen_hashes.
        let per_slot_full_count = results
            .iter()
            .filter(|r| **r == PendingResult::PerSlotFull)
            .count();
        let duplicate_count = results
            .iter()
            .filter(|r| **r == PendingResult::Duplicate)
            .count();

        assert!(
            per_slot_full_count >= 1,
            "At least one thread should hit PerSlotFull, got {per_slot_full_count}"
        );
        assert_eq!(
            per_slot_full_count + duplicate_count,
            NUM_THREADS,
            "All results should be PerSlotFull or Duplicate"
        );

        // After the race, the hash should be cleaned up (not poisoned).
        // Purge and re-add to verify.
        pending.purge_slots_below(102);
        pending.set_current_slot(100);
        assert_eq!(
            pending.add(101, env2),
            PendingResult::Added,
            "Hash must not be poisoned in seen_hashes after PerSlotFull cleanup"
        );
    }

    /// Regression test for #2476: concurrent adds to many new slots must
    /// respect the max_slots cap. With the old DashMap implementation, the
    /// non-atomic len() check + entry() insertion allowed overshoot.
    #[test]
    fn test_issue_2476_concurrent_max_slots_enforcement() {
        use std::sync::Barrier;
        use std::thread;

        const MAX_SLOTS: usize = 5;
        const NUM_THREADS: usize = 20;

        let pending = PendingEnvelopes::new(PendingConfig {
            max_slots: MAX_SLOTS,
            max_age: Duration::from_secs(300),
            max_envelopes_per_slot: MAX_ENVELOPES_PER_SLOT,
        });
        pending.set_current_slot(0);

        let barrier = Barrier::new(NUM_THREADS);

        // Spawn threads each trying to add an envelope for a unique slot.
        thread::scope(|s| {
            let pending_ref = &pending;
            let barrier_ref = &barrier;

            let handles: Vec<_> = (0..NUM_THREADS)
                .map(|i| {
                    s.spawn(move || {
                        let slot = (i as u64) + 1; // slots 1..=20
                        let envelope = make_test_envelope_with_node(slot, (i + 1) as u8);
                        barrier_ref.wait(); // maximize contention
                        pending_ref.add(slot, envelope)
                    })
                })
                .collect();

            let results: Vec<PendingResult> =
                handles.into_iter().map(|h| h.join().unwrap()).collect();

            let added = results
                .iter()
                .filter(|r| **r == PendingResult::Added)
                .count();
            let buffer_full = results
                .iter()
                .filter(|r| **r == PendingResult::BufferFull)
                .count();

            // Exactly max_slots should be added, rest should be BufferFull.
            assert_eq!(
                added, MAX_SLOTS,
                "Exactly max_slots envelopes should be added (got {added})"
            );
            assert_eq!(
                buffer_full,
                NUM_THREADS - MAX_SLOTS,
                "Remaining should be BufferFull (got {buffer_full})"
            );
        });

        // Post-join assertions: slot_count must exactly equal max_slots.
        assert_eq!(
            pending.slot_count(),
            MAX_SLOTS,
            "slot_count must not exceed max_slots after concurrent adds"
        );
    }

    /// Boundary test: max_slots = 0 rejects all new-slot insertions.
    #[test]
    fn test_max_slots_zero_rejects_all() {
        let pending = PendingEnvelopes::new(PendingConfig {
            max_slots: 0,
            max_age: Duration::from_secs(300),
            max_envelopes_per_slot: MAX_ENVELOPES_PER_SLOT,
        });
        pending.set_current_slot(0);

        let result = pending.add(1, make_test_envelope(1));
        assert_eq!(result, PendingResult::BufferFull);
        assert_eq!(pending.slot_count(), 0);
    }
}
