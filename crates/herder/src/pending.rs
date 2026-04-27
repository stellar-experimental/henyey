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
//! which removes entire slots outside the active window. Henyey matches this:
//! no per-slot cap, slot-count gating only for new slots, and
//! `purge_slots_below` for lifecycle cleanup.

use dashmap::DashMap;
use henyey_common::Hash256;
use henyey_scp::SlotIndex;
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use stellar_xdr::curr::ScpEnvelope;

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
#[derive(Debug, Clone)]
pub struct PendingConfig {
    /// Maximum number of slots to buffer.
    pub max_slots: usize,
    /// Maximum age of pending envelopes before eviction.
    pub max_age: Duration,
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
        }
    }
}

/// A pending SCP envelope with metadata.
#[derive(Debug, Clone)]
pub struct PendingEnvelope {
    /// The SCP envelope.
    pub envelope: ScpEnvelope,
    /// When this envelope was received.
    pub received_at: Instant,
    /// Hash of the envelope for deduplication.
    pub hash: Hash256,
}

impl PendingEnvelope {
    /// Create a new pending envelope.
    pub fn new(envelope: ScpEnvelope) -> Self {
        // Compute envelope hash for deduplication
        let hash = Hash256::hash_xdr(&envelope);
        Self {
            envelope,
            received_at: Instant::now(),
            hash,
        }
    }

    /// Check if this envelope has expired.
    pub fn is_expired(&self, max_age: Duration) -> bool {
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
    /// Buffer is full.
    BufferFull,
}

/// Manages pending SCP envelopes for future slots.
///
/// When the node is catching up or when envelopes arrive for future slots,
/// they are buffered here until the slot becomes active.
pub struct PendingEnvelopes {
    /// Configuration.
    config: PendingConfig,
    /// Pending envelopes organized by slot.
    slots: DashMap<SlotIndex, Vec<PendingEnvelope>>,
    /// Seen envelope hashes for deduplication.
    seen_hashes: DashMap<Hash256, ()>,
    /// Current active slot.
    current_slot: RwLock<SlotIndex>,
    /// Statistics.
    stats: RwLock<PendingStats>,
    /// Last slot for which a BufferFull warning was emitted (rate-limiting).
    last_buffer_full_warn_slot: AtomicU64,
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
    /// High-water mark: largest envelope count observed in any single slot.
    pub max_envelopes_per_slot: u64,
}

impl PendingEnvelopes {
    /// Create a new pending envelope manager.
    pub fn new(config: PendingConfig) -> Self {
        Self {
            config,
            slots: DashMap::new(),
            seen_hashes: DashMap::new(),
            current_slot: RwLock::new(0),
            stats: RwLock::new(PendingStats::default()),
            last_buffer_full_warn_slot: AtomicU64::new(0),
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

        // Check for duplicate
        if self.seen_hashes.contains_key(&pending.hash) {
            self.stats.write().duplicates += 1;
            return PendingResult::Duplicate;
        }

        // Existing slot — just append, no slot-count check needed.
        // This avoids false BufferFull when max_slots slots exist but
        // the target slot is already one of them.
        if let Some(mut existing) = self.slots.get_mut(&slot) {
            self.seen_hashes.insert(pending.hash, ());
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
        if self.slots.len() >= self.config.max_slots {
            self.evict_old_slots(current);
            if self.slots.len() >= self.config.max_slots {
                let mut stats = self.stats.write();
                stats.buffer_full += 1;
                return PendingResult::BufferFull;
            }
        }

        // Insert into new slot.
        self.seen_hashes.insert(pending.hash, ());
        self.slots.entry(slot).or_default().push(pending);

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
    fn release(&self, slot: SlotIndex) -> Vec<ScpEnvelope> {
        if let Some((_, envelopes)) = self.slots.remove(&slot) {
            let count = envelopes.len() as u64;
            self.stats.write().released += count;

            // Remove from seen hashes
            for env in &envelopes {
                self.seen_hashes.remove(&env.hash);
            }

            envelopes
                .into_iter()
                .rev()
                .filter(|e| !e.is_expired(self.config.max_age))
                .map(|e| e.envelope)
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
        let mut result = BTreeMap::new();
        let slots_to_release: Vec<SlotIndex> = self
            .slots
            .iter()
            .filter(|e| *e.key() <= slot)
            .map(|e| *e.key())
            .collect();

        for s in slots_to_release {
            let envelopes = self.release(s);
            if !envelopes.is_empty() {
                result.insert(s, envelopes);
            }
        }

        result
    }

    /// Evict old slots that are behind the current slot.
    fn evict_old_slots(&self, current: SlotIndex) {
        let old_slots: Vec<SlotIndex> = self
            .slots
            .iter()
            .filter(|e| *e.key() < current)
            .map(|e| *e.key())
            .collect();

        for slot in old_slots {
            if let Some((_, envelopes)) = self.slots.remove(&slot) {
                let count = envelopes.len() as u64;
                self.stats.write().evicted += count;

                for env in envelopes {
                    self.seen_hashes.remove(&env.hash);
                }
            }
        }
    }

    /// Evict expired envelopes from all slots.
    pub fn evict_expired(&self) {
        let max_age = self.config.max_age;

        for mut entry in self.slots.iter_mut() {
            let initial_len = entry.len();

            // Collect hashes of expired envelopes
            let expired_hashes: Vec<Hash256> = entry
                .iter()
                .filter(|e| e.is_expired(max_age))
                .map(|e| e.hash)
                .collect();

            // Remove expired envelopes
            entry.retain(|e| !e.is_expired(max_age));

            let removed = initial_len - entry.len();
            if removed > 0 {
                self.stats.write().evicted += removed as u64;

                // Remove from seen hashes
                for hash in expired_hashes {
                    self.seen_hashes.remove(&hash);
                }
            }
        }

        // Remove empty slots
        self.slots.retain(|_, v| !v.is_empty());
    }

    /// Get the number of pending envelopes.
    pub fn len(&self) -> usize {
        self.slots.iter().map(|e| e.len()).sum()
    }

    /// Check if there are no pending envelopes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the number of slots with pending envelopes.
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Get statistics.
    pub fn stats(&self) -> PendingStats {
        self.stats.read().clone()
    }

    /// Check if there are pending envelopes for a slot.
    pub fn has_pending(&self, slot: SlotIndex) -> bool {
        self.slots
            .get(&slot)
            .map(|e| !e.is_empty())
            .unwrap_or(false)
    }

    /// Get the count of pending envelopes for a slot.
    pub fn pending_count(&self, slot: SlotIndex) -> usize {
        self.slots.get(&slot).map(|e| e.len()).unwrap_or(0)
    }

    /// Clear all pending envelopes.
    pub fn clear(&self) {
        let slots_count = self.slots.len();
        let seen_count = self.seen_hashes.len();
        self.slots.clear();
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
        let slots_to_remove: Vec<SlotIndex> = self
            .slots
            .iter()
            .filter(|e| *e.key() < min_slot)
            .map(|e| *e.key())
            .collect();

        let mut total_evicted = 0u64;
        for slot in slots_to_remove {
            if let Some((_, envelopes)) = self.slots.remove(&slot) {
                total_evicted += envelopes.len() as u64;
                for env in envelopes {
                    self.seen_hashes.remove(&env.hash);
                }
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
}

impl Default for PendingEnvelopes {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
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
            signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
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
            signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
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
    /// in one slot should all be accepted (no per-slot cap).
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
}
