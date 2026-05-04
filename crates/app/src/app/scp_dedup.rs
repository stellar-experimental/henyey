//! In-flight SCP envelope dedup cache.
//!
//! Mirrors stellar-core's `mScheduledMessages`
//! (`OverlayManagerImpl.cpp:326, 1190-1212`). Entries are inserted after
//! successful pre-filter + dispatch to the verify worker and removed at
//! the start of `process_verified`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use henyey_common::Hash256;
use stellar_xdr::curr::{Limits, ScpEnvelope, WriteXdr};

/// In-flight SCP envelope dedup cache.
///
/// Tracks envelope hashes that have been dispatched to the signature
/// verification worker but not yet processed by [`super::App::process_verified`].
/// Duplicate envelopes are rejected (and counted) so the verifier is not
/// overwhelmed by repeated network deliveries of the same message.
pub(crate) struct ScheduledEnvelopeSet {
    cache: Mutex<HashSet<Hash256>>,
    dedup_count: AtomicU64,
}

impl ScheduledEnvelopeSet {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashSet::new()),
            dedup_count: AtomicU64::new(0),
        }
    }

    /// Compute the dedup hash for an SCP envelope.
    ///
    /// Uses the same serialisation + blake2 scheme as stellar-core's
    /// `checkScheduledAndCache` (Peer.cpp:1113-1117).
    pub fn envelope_hash(envelope: &ScpEnvelope) -> Hash256 {
        henyey_crypto::blake2(&envelope.to_xdr(Limits::none()).unwrap())
    }

    /// Check whether `hash` is already in-flight.
    ///
    /// Returns `None` (and increments the dedup counter) if the hash is
    /// already cached — the caller should drop the envelope.
    ///
    /// Returns `Some(DedupSlot)` if the hash is new. The caller must call
    /// [`DedupSlot::commit`] after successful dispatch to the verify
    /// worker, or simply drop the slot to abort without poisoning the
    /// cache (matching the pre-filter-reject path).
    ///
    /// The check → commit gap is intentional: it mirrors the production
    /// event-loop pattern where the pre-filter runs between check and
    /// commit on the single event-loop task. No concurrent caller can
    /// race between `check()` and `commit()`.
    pub fn check(&self, hash: &Hash256) -> Option<DedupSlot<'_>> {
        let guard = self.cache.lock().unwrap();
        if guard.contains(hash) {
            drop(guard);
            self.dedup_count.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        drop(guard);
        Some(DedupSlot {
            set: self,
            hash: *hash,
        })
    }

    /// Remove an envelope from the in-flight set.
    ///
    /// Called at the **start** of `process_verified`, before any early
    /// returns, so cleanup is guaranteed even for `InvalidSignature` or
    /// `Panic` verdicts.
    pub fn complete(&self, envelope: &ScpEnvelope) {
        let hash = Self::envelope_hash(envelope);
        self.cache.lock().unwrap().remove(&hash);
    }

    /// Number of envelopes rejected by the dedup check since startup.
    pub fn dedup_count(&self) -> u64 {
        self.dedup_count.load(Ordering::Relaxed)
    }

    /// Number of envelopes currently in the in-flight set.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.cache.lock().unwrap().len()
    }
}

/// A slot representing an envelope that passed the dedup check but has
/// not yet been committed (dispatched) to the verify worker.
///
/// If dropped without calling [`commit`](DedupSlot::commit), no cache
/// entry is created — matching the pre-filter reject and channel-closed
/// paths where the envelope never reaches the worker.
pub(crate) struct DedupSlot<'a> {
    set: &'a ScheduledEnvelopeSet,
    hash: Hash256,
}

impl DedupSlot<'_> {
    /// Insert into the in-flight cache.
    ///
    /// Call this **after** successful dispatch (channel send) to the
    /// verify worker.
    pub fn commit(self) {
        self.set.cache.lock().unwrap().insert(self.hash);
    }
}

// ───────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        NodeId, PublicKey as XdrPublicKey, ScpBallot, ScpStatement, ScpStatementPledges,
        ScpStatementPrepare, Signature, Uint256, Value,
    };

    fn make_envelope(slot: u64, node_seed: u8) -> ScpEnvelope {
        let node_id = NodeId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([node_seed; 32])));
        let value = Value(vec![].try_into().unwrap());
        ScpEnvelope {
            statement: ScpStatement {
                node_id,
                slot_index: slot,
                pledges: ScpStatementPledges::Prepare(ScpStatementPrepare {
                    quorum_set_hash: stellar_xdr::curr::Hash([0u8; 32]),
                    ballot: ScpBallot {
                        counter: 1,
                        value: value.clone(),
                    },
                    prepared: None,
                    prepared_prime: None,
                    n_c: 0,
                    n_h: 0,
                }),
            },
            signature: Signature(vec![0u8; 64].try_into().unwrap()),
        }
    }

    #[test]
    fn test_dedup_rejects_inflight_duplicate() {
        let set = ScheduledEnvelopeSet::new();
        let env = make_envelope(100, 1);
        let hash = ScheduledEnvelopeSet::envelope_hash(&env);

        // First check: new envelope, returns Some.
        let slot = set.check(&hash).expect("first check should pass");
        slot.commit();

        // Second check: duplicate, returns None.
        assert!(set.check(&hash).is_none());
        assert_eq!(set.dedup_count(), 1);
    }

    #[test]
    fn test_dedup_allows_after_completion() {
        let set = ScheduledEnvelopeSet::new();
        let env = make_envelope(100, 1);
        let hash = ScheduledEnvelopeSet::envelope_hash(&env);

        // Dispatch.
        set.check(&hash).unwrap().commit();
        assert_eq!(set.len(), 1);

        // Complete (simulate process_verified).
        set.complete(&env);
        assert_eq!(set.len(), 0);

        // Re-dispatch: passes again.
        assert!(set.check(&hash).is_some());
    }

    #[test]
    fn test_dedup_no_poison_on_drop() {
        let set = ScheduledEnvelopeSet::new();
        let env = make_envelope(100, 1);
        let hash = ScheduledEnvelopeSet::envelope_hash(&env);

        // Check passes but slot is dropped (pre-filter reject).
        let slot = set.check(&hash).expect("check should pass");
        drop(slot);

        // Cache is not poisoned.
        assert_eq!(set.len(), 0);
        assert!(set.check(&hash).is_some());
    }

    #[test]
    fn test_dedup_cleanup_deterministic() {
        let set = ScheduledEnvelopeSet::new();
        let env = make_envelope(100, 1);
        let hash = ScheduledEnvelopeSet::envelope_hash(&env);

        set.check(&hash).unwrap().commit();
        assert_eq!(set.len(), 1);

        set.complete(&env);
        assert_eq!(set.len(), 0);
    }

    #[test]
    fn test_dedup_distinct_envelopes() {
        let set = ScheduledEnvelopeSet::new();
        let env_a = make_envelope(100, 1);
        let env_b = make_envelope(101, 2);
        let hash_a = ScheduledEnvelopeSet::envelope_hash(&env_a);
        let hash_b = ScheduledEnvelopeSet::envelope_hash(&env_b);

        assert_ne!(hash_a, hash_b);

        set.check(&hash_a).unwrap().commit();
        set.check(&hash_b).unwrap().commit();

        assert_eq!(set.len(), 2);
        assert_eq!(set.dedup_count(), 0);
    }

    #[test]
    fn test_dedup_hash_deterministic() {
        let env = make_envelope(42, 7);
        let h1 = ScheduledEnvelopeSet::envelope_hash(&env);
        let h2 = ScheduledEnvelopeSet::envelope_hash(&env);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_dedup_counter_multiple_duplicates() {
        let set = ScheduledEnvelopeSet::new();
        let env = make_envelope(100, 1);
        let hash = ScheduledEnvelopeSet::envelope_hash(&env);

        set.check(&hash).unwrap().commit();

        for _ in 0..5 {
            assert!(set.check(&hash).is_none());
        }
        assert_eq!(set.dedup_count(), 5);
    }

    #[test]
    fn test_dedup_channel_closed_leaves_set_clean() {
        let set = ScheduledEnvelopeSet::new();
        let env = make_envelope(100, 1);
        let hash = ScheduledEnvelopeSet::envelope_hash(&env);

        // Check passes, but simulate channel-closed: drop slot without commit.
        let slot = set.check(&hash).expect("check should pass");
        // Simulating: permit_res = verifier.tx.reserve() => Err(_closed)
        drop(slot);

        assert_eq!(set.len(), 0);
        // The envelope can be retried later.
        assert!(set.check(&hash).is_some());
    }
}
