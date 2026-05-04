//! Flood gate for managing message propagation and duplicate detection.
//!
//! The Stellar overlay network propagates certain message types (transactions,
//! SCP messages, etc.) to all connected peers. To prevent infinite loops and
//! reduce bandwidth, the [`FloodGate`] tracks which messages have been seen
//! and from which peers.
//!
//! # Functionality
//!
//! - **Duplicate Detection**: Messages are identified by their BLAKE2b-256 hash
//!   (matching stellar-core's `xdrBlake2`). If we've seen a message before, it's not flooded again.
//!
//! - **Peer Tracking**: Records which peers have sent each message, so we
//!   don't forward messages back to peers that already have them.
//!
//! - **Ledger-boundary Cleanup**: Entries are removed at ledger close via
//!   [`FloodGate::clear_below`], matching stellar-core's `clearBelow()`.
//!   A secondary TTL check removes stale entries as a defensive measure.
//!
//! - **Rate Limiting**: Soft limit on messages per second to prevent
//!   overwhelming the node during traffic spikes.

use dashmap::DashMap;
use henyey_common::Hash256;
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use stellar_xdr::curr::StellarMessage;
use tracing::{debug, trace, warn};

use crate::PeerId;

/// Result of recording a message hash in the [`FloodGate`].
///
/// **FloodGate is relay accounting, not a generic dedup layer.** The caller
/// decides drop policy per message type:
/// - SCP messages are NEVER dropped based on this (stellar-core parity:
///   Peer.cpp:1667-1673 calls `recvFloodedMsgID` then unconditionally
///   calls `recvSCPEnvelope`).
/// - Transaction duplicates may be dropped (current `peer_loop` behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum RelayRecord {
    /// First time this hash was seen — caller may forward to other peers.
    New,
    /// Hash was already recorded. Caller decides drop policy per message type.
    Repeated,
}

impl RelayRecord {
    /// Returns `true` if this is the first time the hash was recorded.
    pub fn is_new(self) -> bool {
        matches!(self, RelayRecord::New)
    }
}

/// Default TTL for seen messages (5 minutes).
///
/// Used by [`FloodGate::clear_below`] as a secondary expiry mechanism
/// during ledger-boundary cleanup.
const DEFAULT_TTL_SECS: u64 = 300;

/// Threshold for warning about large flood gate size.
///
/// If the map exceeds this many entries between ledger closes, a one-shot
/// warning is logged to alert operators.
const LARGE_MAP_WARN_THRESHOLD: usize = 500_000;

/// Default global rate limit (messages per second).
///
/// This is a node-wide aggregate backstop against Sybil attacks.
/// Per-peer rate limiting (in peer_loop.rs) is the primary enforcement;
/// this global limit is an emergency failsafe.
const DEFAULT_RATE_LIMIT_PER_SEC: u64 = 5000;

/// Internal tracking entry for a seen message.
struct SeenEntry {
    /// When the message was first seen.
    first_seen: Instant,
    /// Ledger sequence when the message was first seen.
    ledger_seq: u32,
    /// Set of peers that have sent us this message.
    peers: HashSet<PeerId>,
}

impl SeenEntry {
    /// Creates a new entry with the current timestamp and ledger sequence.
    fn new(ledger_seq: u32) -> Self {
        Self {
            first_seen: Instant::now(),
            ledger_seq,
            peers: HashSet::new(),
        }
    }

    /// Records that a peer has sent this message.
    fn add_peer(&mut self, peer: PeerId) {
        self.peers.insert(peer);
    }

    /// Returns true if this entry has exceeded its TTL.
    fn is_expired(&self, ttl: Duration) -> bool {
        self.first_seen.elapsed() > ttl
    }
}

/// Flood gate for tracking seen messages and preventing duplicates.
///
/// The flood gate is the core of the overlay's message propagation system.
/// It ensures that each unique message is only flooded once, while tracking
/// which peers have already received each message.
///
/// # Thread Safety
///
/// All operations are thread-safe and can be called concurrently from
/// multiple peer message handlers.
///
/// # Example
///
/// ```rust,ignore
/// let gate = FloodGate::new();
///
/// // Record a message and check whether it was new
/// let hash = compute_message_hash(&message);
/// let result = gate.record_seen(hash, Some(peer_id), current_ledger_seq);
/// if result.is_new() {
///     // First time seeing this — flood to other peers
///     let forward_to = gate.get_forward_peers(&hash, &all_peers);
/// }
/// ```
pub struct FloodGate {
    /// Map of message hash to tracking entry.
    seen: DashMap<Hash256, SeenEntry>,
    /// Time-to-live for message entries (used by `clear_below`).
    ttl: Duration,
    /// Counter: total messages processed.
    messages_seen: AtomicU64,
    /// Counter: duplicate messages observed.
    messages_duplicate: AtomicU64,
    /// Maximum messages per second.
    rate_limit: u64,
    /// Start of current rate-limiting window.
    rate_window_start: RwLock<Instant>,
    /// Messages counted in current window.
    rate_window_count: AtomicU64,
    /// Whether the large-map warning has already been emitted.
    large_map_warned: AtomicBool,
}

impl FloodGate {
    /// Creates a new flood gate with default settings (5 minute TTL).
    pub fn new() -> Self {
        Self::with_ttl(Duration::from_secs(DEFAULT_TTL_SECS))
    }

    /// Creates a new flood gate with a custom TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            seen: DashMap::new(),
            ttl,
            messages_seen: AtomicU64::new(0),
            messages_duplicate: AtomicU64::new(0),
            rate_limit: DEFAULT_RATE_LIMIT_PER_SEC,
            rate_window_start: RwLock::new(Instant::now()),
            rate_window_count: AtomicU64::new(0),
            large_map_warned: AtomicBool::new(false),
        }
    }

    /// Returns true if this message has not been seen before.
    ///
    /// This is a quick check that doesn't record the message - use
    /// [`record_seen`](FloodGate::record_seen) to both check and record.
    pub fn should_flood(&self, message_hash: &Hash256) -> bool {
        !self.seen.contains_key(message_hash)
    }

    /// Records that a message has been seen, optionally from a specific peer.
    ///
    /// Returns [`RelayRecord::New`] if this is the first time seeing this
    /// message, or [`RelayRecord::Repeated`] if it was already recorded.
    ///
    /// **Callers must NOT use this return value as a drop signal.** FloodGate
    /// is relay accounting — it tracks which peers have sent us a given hash
    /// so `get_forward_peers` can avoid echoing it back. Dedup decisions
    /// belong to downstream, message-type-specific handlers (SCP scheduler,
    /// tx queue, etc.). See issues #2317, #2327.
    ///
    /// If `from_peer` is `Some`, that peer is recorded so we don't forward
    /// the message back to them.
    ///
    /// The `ledger_seq` parameter records the current ledger sequence for
    /// ledger-based cleanup via [`clear_below`](FloodGate::clear_below).
    ///
    /// This is a pure insert/lookup operation with no automatic cleanup,
    /// matching stellar-core's `addRecord()`. Cleanup happens at ledger
    /// boundaries via [`clear_below`](FloodGate::clear_below).
    ///
    /// # Relay tracking vs. in-flight dedup
    ///
    /// `FloodGate` is a **relay-tracking** structure (the henyey equivalent
    /// of stellar-core's `recvFloodedMsgID` / `addRecord` path). Its job
    /// is to remember which peers have sent us each message hash so that
    /// [`get_forward_peers`](FloodGate::get_forward_peers) does not echo
    /// messages back to their senders, and so that operators can observe
    /// duplicate-receive rates via metrics.
    ///
    /// It is **not** a substitute for short-lived in-flight dedup. Entries
    /// here persist for an entire ledger window (cleared at ledger close
    /// by `clear_below`, with a TTL backstop), whereas stellar-core's
    /// in-flight dedup (`checkScheduledAndCache`, Peer.cpp:1113-1117)
    /// uses a `weak_ptr<CapacityTrackedMessage>` cache that releases
    /// entries the moment processing completes — typically milliseconds.
    ///
    /// **No FloodGate-tracked message type should be dropped based on
    /// `record_seen`'s return value**:
    /// - SCP: Self-broadcast records a hash with `from_peer = None`. If a
    ///   peer later echoes the same envelope back, the herder still needs
    ///   that envelope (with peer provenance) to fetch tx-sets and converge.
    /// - Tx: stellar-core's `OverlayManagerImpl::recvTransaction`
    ///   (OverlayManagerImpl.cpp:1215-1248) calls `recvFloodedMsgID` for
    ///   relay tracking then unconditionally processes the transaction.
    ///
    /// stellar-core parity:
    /// - SCP: Peer.cpp:1667-1673 calls `recvFloodedMsgID` then
    ///   unconditionally calls `recvSCPEnvelope`.
    /// - Tx: OverlayManagerImpl.cpp:1224-1229 calls `recvFloodedMsgID`
    ///   then unconditionally calls `Herder::recvTransaction`.
    /// See issues #2317, #2327.
    pub fn record_seen(
        &self,
        message_hash: Hash256,
        from_peer: Option<PeerId>,
        ledger_seq: u32,
    ) -> RelayRecord {
        self.messages_seen.fetch_add(1, Ordering::Relaxed);

        // Check if we've seen this message
        if let Some(mut entry) = self.seen.get_mut(&message_hash) {
            // Already seen, record the peer
            if let Some(peer) = from_peer {
                entry.add_peer(peer);
            }
            self.messages_duplicate.fetch_add(1, Ordering::Relaxed);
            trace!("Duplicate message: {}", message_hash);
            return RelayRecord::Repeated;
        }

        // New message
        let mut entry = SeenEntry::new(ledger_seq);
        if let Some(peer) = from_peer {
            entry.add_peer(peer);
        }
        self.seen.insert(message_hash, entry);

        // One-shot warning if the map grows unexpectedly large between ledger closes
        let len = self.seen.len();
        if len > LARGE_MAP_WARN_THRESHOLD && !self.large_map_warned.swap(true, Ordering::Relaxed) {
            warn!(
                entries = len,
                "FloodGate map exceeds {} entries; cleanup happens at ledger close",
                LARGE_MAP_WARN_THRESHOLD
            );
        }

        trace!("New message: {}", message_hash);
        RelayRecord::New
    }

    /// Checks if another message is allowed under the rate limit.
    ///
    /// Returns `true` if we're within the rate limit, `false` if we've
    /// exceeded it and should drop the message.
    pub fn allow_message(&self) -> bool {
        let now = Instant::now();
        {
            let mut start = self.rate_window_start.write();
            if now.duration_since(*start) >= Duration::from_secs(1) {
                *start = now;
                self.rate_window_count.store(0, Ordering::Relaxed);
            }
        }

        let count = self.rate_window_count.fetch_add(1, Ordering::Relaxed) + 1;
        count <= self.rate_limit
    }

    /// Returns the list of peers to forward a message to.
    ///
    /// Excludes any peers that have already sent us this message (tracked
    /// via [`record_seen`](FloodGate::record_seen)).
    pub fn get_forward_peers(&self, message_hash: &Hash256, all_peers: &[PeerId]) -> Vec<PeerId> {
        let exclude: HashSet<PeerId> = self
            .seen
            .get(message_hash)
            .map(|entry| entry.peers.iter().cloned().collect())
            .unwrap_or_default();

        all_peers
            .iter()
            .filter(|p| !exclude.contains(*p))
            .cloned()
            .collect()
    }

    /// Returns true if this message has been seen before.
    pub fn has_seen(&self, message_hash: &Hash256) -> bool {
        self.seen.contains_key(message_hash)
    }

    /// Removes a previously-seen message from the flood gate, allowing
    /// it to be treated as new on re-delivery.
    ///
    /// Mirrors stellar-core's `Floodgate::forgetRecord(Hash const& h)`
    /// (Floodgate.cpp:197-200). Called when a flood-tracked message is
    /// discarded after initial recording — e.g., SCP envelopes rejected
    /// by herder pre-filter or post-verify gate drift.
    pub fn forget(&self, message_hash: &Hash256) {
        self.seen.remove(message_hash);
    }

    /// Returns current statistics about the flood gate.
    pub fn stats(&self) -> FloodGateStats {
        FloodGateStats {
            seen_count: self.seen.len(),
            total_messages: self.messages_seen.load(Ordering::Relaxed),
            duplicate_messages: self.messages_duplicate.load(Ordering::Relaxed),
        }
    }

    /// Removes flood records from ledgers before `ledger_seq`.
    ///
    /// Matches upstream stellar-core's `clearBelow(maxLedger)` which removes
    /// records from ledgers before `maxLedger`. Additionally removes
    /// TTL-expired entries as a henyey-specific defensive measure (stellar-core's
    /// `clearBelow` is purely ledger-based).
    pub fn clear_below(&self, ledger_seq: u32) {
        let ttl = self.ttl;
        let before_count = self.seen.len();
        self.seen
            .retain(|_, entry| entry.ledger_seq >= ledger_seq && !entry.is_expired(ttl));
        let removed = before_count.saturating_sub(self.seen.len());

        if removed > 0 {
            debug!(
                "FloodGate clear_below({}): removed {} entries",
                ledger_seq, removed
            );
        }

        // Reset the large-map warning so it can fire again if growth recurs
        self.large_map_warned.store(false, Ordering::Relaxed);
    }

    /// Clears all entries from the flood gate.
    ///
    /// Use with caution - this will allow previously-seen messages to be
    /// flooded again.
    pub fn clear(&self) {
        self.seen.clear();
        self.large_map_warned.store(false, Ordering::Relaxed);
    }
}

impl Default for FloodGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics snapshot from a [`FloodGate`].
#[derive(Debug, Clone)]
pub struct FloodGateStats {
    /// Number of unique messages currently being tracked.
    pub seen_count: usize,
    /// Total messages processed (including duplicates).
    pub total_messages: u64,
    /// Number of duplicate messages observed (relay accounting only).
    pub duplicate_messages: u64,
}

impl FloodGateStats {
    /// Calculates the duplicate rate as a percentage.
    ///
    /// Returns 0.0 if no messages have been processed.
    pub fn duplicate_rate(&self) -> f64 {
        if self.total_messages == 0 {
            0.0
        } else {
            (self.duplicate_messages as f64 / self.total_messages as f64) * 100.0
        }
    }
}

/// Computes the BLAKE2b-256 hash of a message for flood tracking.
///
/// This matches stellar-core's `xdrBlake2()` used in `Floodgate::broadcast()`.
pub fn compute_message_hash(message: &StellarMessage) -> Hash256 {
    use stellar_xdr::curr::{Limits, WriteXdr};
    let bytes = message
        .to_xdr(Limits::none())
        .expect("XDR serialization of StellarMessage must not fail");
    henyey_crypto::blake2(&bytes)
}

/// A message queued for flooding, with tracking metadata.
///
/// Used internally to track messages that need to be forwarded to peers.
pub struct FloodRecord {
    /// BLAKE2b-256 hash of the message.
    pub hash: Hash256,
    /// The message to be flooded.
    pub message: StellarMessage,
    /// When the message was received.
    pub received: Instant,
    /// The peer that sent us this message (if any).
    pub from_peer: Option<PeerId>,
}

impl FloodRecord {
    /// Creates a new flood record for a message.
    pub fn new(message: StellarMessage, from_peer: Option<PeerId>) -> Self {
        let hash = compute_message_hash(&message);
        Self {
            hash,
            message,
            received: Instant::now(),
            from_peer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_hash(v: u8) -> Hash256 {
        Hash256([v; 32])
    }

    fn make_peer_id(v: u8) -> PeerId {
        PeerId::from_bytes([v; 32])
    }

    #[test]
    fn test_flood_gate_basic() {
        let gate = FloodGate::new();

        let hash = make_hash(1);
        assert!(gate.should_flood(&hash));

        // Record as seen
        assert_eq!(gate.record_seen(hash, None, 1), RelayRecord::New);

        // Should not flood again
        assert!(!gate.should_flood(&hash));

        // Record again should return Repeated
        assert_eq!(gate.record_seen(hash, None, 1), RelayRecord::Repeated);
    }

    #[test]
    fn test_flood_gate_with_peers() {
        let gate = FloodGate::new();

        let hash = make_hash(1);
        let peer1 = make_peer_id(1);
        let peer2 = make_peer_id(2);
        let peer3 = make_peer_id(3);

        // First seen from peer1
        assert_eq!(
            gate.record_seen(hash, Some(peer1.clone()), 1),
            RelayRecord::New
        );

        // Also seen from peer2
        assert_eq!(
            gate.record_seen(hash, Some(peer2.clone()), 1),
            RelayRecord::Repeated
        );

        // Get forward peers - should exclude peer1 and peer2
        let all_peers = vec![peer1.clone(), peer2.clone(), peer3.clone()];
        let forward = gate.get_forward_peers(&hash, &all_peers);

        assert_eq!(forward.len(), 1);
        assert_eq!(forward[0], peer3);
    }

    #[test]
    fn test_flood_gate_stats() {
        let gate = FloodGate::new();

        let hash1 = make_hash(1);
        let hash2 = make_hash(2);

        let _ = gate.record_seen(hash1, None, 1);
        let _ = gate.record_seen(hash1, None, 1); // duplicate
        let _ = gate.record_seen(hash2, None, 1);

        let stats = gate.stats();
        assert_eq!(stats.seen_count, 2);
        assert_eq!(stats.total_messages, 3);
        assert_eq!(stats.duplicate_messages, 1);
    }

    #[test]
    fn test_flood_gate_expiry() {
        let gate = FloodGate::with_ttl(Duration::from_millis(10));

        let hash = make_hash(1);
        let _ = gate.record_seen(hash, None, 1);
        std::thread::sleep(Duration::from_millis(20));

        // clear_below with a high ledger seq removes expired entries
        gate.clear_below(u32::MAX);

        // Should be able to flood again
        assert!(gate.should_flood(&hash));
    }

    #[test]
    fn test_flood_record() {
        let message = StellarMessage::Peers(stellar_xdr::curr::VecM::default());
        let record = FloodRecord::new(message, None);

        assert!(!record.hash.is_zero());
        assert!(record.from_peer.is_none());
    }

    #[test]
    fn test_clear_below_removes_by_ledger() {
        let gate = FloodGate::with_ttl(Duration::from_secs(300));

        let hash1 = make_hash(1);
        let hash2 = make_hash(2);
        let hash3 = make_hash(3);
        // Record at different ledger sequences
        let _ = gate.record_seen(hash1, None, 50);
        let _ = gate.record_seen(hash2, None, 100);
        let _ = gate.record_seen(hash3, None, 150);

        assert_eq!(gate.stats().seen_count, 3);

        // clear_below(100) removes entries from ledgers < 100
        gate.clear_below(100);
        assert_eq!(gate.stats().seen_count, 2);
        assert!(!gate.has_seen(&hash1));
        assert!(gate.has_seen(&hash2));
        assert!(gate.has_seen(&hash3));
    }

    #[test]
    fn test_clear_below_removes_expired() {
        // Use a very short TTL so entries expire quickly
        let gate = FloodGate::with_ttl(Duration::from_millis(10));

        let hash1 = make_hash(1);
        let hash2 = make_hash(2);
        let _ = gate.record_seen(hash1, None, 100);
        let _ = gate.record_seen(hash2, None, 100);

        assert_eq!(gate.stats().seen_count, 2);

        // Wait for entries to expire
        std::thread::sleep(Duration::from_millis(20));

        // clear_below triggers cleanup of expired entries (even at same ledger)
        gate.clear_below(100);

        assert_eq!(gate.stats().seen_count, 0);
    }

    #[test]
    fn test_clear_below_preserves_recent() {
        // Use a long TTL so entries don't expire
        let gate = FloodGate::with_ttl(Duration::from_secs(300));

        let hash1 = make_hash(1);
        let hash2 = make_hash(2);
        let _ = gate.record_seen(hash1, None, 100);
        let _ = gate.record_seen(hash2, None, 100);

        // clear_below should not remove entries at or above the threshold
        gate.clear_below(100);

        assert_eq!(gate.stats().seen_count, 2);
        assert!(gate.has_seen(&hash1));
        assert!(gate.has_seen(&hash2));
    }

    /// Regression test for AUDIT-174: record_seen() must NOT trigger automatic
    /// cleanup. Expired entries should only be removed by explicit clear_below().
    #[test]
    fn test_record_seen_does_not_auto_cleanup() {
        // Use a short TTL so entries expire quickly
        let gate = FloodGate::with_ttl(Duration::from_millis(50));

        // Insert 5 entries at ledger 1
        for i in 0..5u8 {
            let _ = gate.record_seen(make_hash(i), None, 1);
        }
        assert_eq!(gate.stats().seen_count, 5);

        // Wait for all entries to expire (2x TTL for generous margin)
        std::thread::sleep(Duration::from_millis(100));

        // Insert 5 new entries at ledger 2 via record_seen
        for i in 10..15u8 {
            let _ = gate.record_seen(make_hash(i), None, 2);
        }

        // All 10 entries should still be present — record_seen does not clean up
        assert_eq!(gate.stats().seen_count, 10);

        // Now clear_below(2) should remove ledger-1 entries (and any expired)
        gate.clear_below(2);
        assert_eq!(gate.stats().seen_count, 5);

        // Only ledger-2 entries remain
        for i in 10..15u8 {
            assert!(gate.has_seen(&make_hash(i)));
        }
        for i in 0..5u8 {
            assert!(!gate.has_seen(&make_hash(i)));
        }
    }

    #[test]
    fn test_flood_gate_not_polluted_by_pull_control() {
        use crate::codec::helpers;

        let gate = FloodGate::new();

        // Simulate the corrected receive path: only record_seen for
        // is_flood_gate_tracked messages.
        let advert = StellarMessage::FloodAdvert(Default::default());
        let demand = StellarMessage::FloodDemand(Default::default());
        let tx = StellarMessage::Transaction(stellar_xdr::curr::TransactionEnvelope::TxV0(
            Default::default(),
        ));

        // Pull-control messages are flood messages but NOT gate-tracked
        assert!(helpers::is_flood_message(&advert));
        assert!(helpers::is_flood_message(&demand));
        assert!(!helpers::is_flood_gate_tracked(&advert));
        assert!(!helpers::is_flood_gate_tracked(&demand));

        // Simulating the fixed routing: only gate-tracked messages get recorded
        if helpers::is_flood_gate_tracked(&advert) {
            let _ = gate.record_seen(compute_message_hash(&advert), None, 1);
        }
        if helpers::is_flood_gate_tracked(&demand) {
            let _ = gate.record_seen(compute_message_hash(&demand), None, 1);
        }
        // FloodGate should be empty — pull-control does NOT pollute it
        assert_eq!(gate.seen.len(), 0);

        // Transaction IS gate-tracked and should be recorded
        assert!(helpers::is_flood_gate_tracked(&tx));
        if helpers::is_flood_gate_tracked(&tx) {
            let _ = gate.record_seen(compute_message_hash(&tx), None, 1);
        }
        assert_eq!(gate.seen.len(), 1);
    }

    #[test]
    fn test_flood_gate_forget_basic() {
        let gate = FloodGate::new();
        let hash = make_hash(1);

        // Record, then forget — should_flood returns true again.
        assert_eq!(gate.record_seen(hash, None, 1), RelayRecord::New);
        assert!(!gate.should_flood(&hash));

        gate.forget(&hash);
        assert!(gate.should_flood(&hash));
        assert!(!gate.has_seen(&hash));
    }

    #[test]
    fn test_flood_gate_forget_nonexistent() {
        let gate = FloodGate::new();
        let hash = make_hash(42);

        // Forgetting a hash that was never recorded is a no-op.
        gate.forget(&hash);
        assert!(gate.should_flood(&hash));
    }

    #[test]
    fn test_flood_gate_forget_redelivery() {
        let gate = FloodGate::new();
        let hash = make_hash(1);
        let peer_a = make_peer_id(1);
        let peer_b = make_peer_id(2);
        let peer_c = make_peer_id(3);
        let all_peers = vec![peer_a.clone(), peer_b.clone(), peer_c.clone()];

        // Peer A delivers the message.
        assert_eq!(
            gate.record_seen(hash, Some(peer_a.clone()), 1),
            RelayRecord::New
        );
        // Forward list excludes peer A.
        let fwd = gate.get_forward_peers(&hash, &all_peers);
        assert!(!fwd.contains(&peer_a));
        assert!(fwd.contains(&peer_b));

        // Forget the record (simulating herder discard).
        gate.forget(&hash);

        // Peer B re-delivers. FloodGate treats it as new.
        assert_eq!(
            gate.record_seen(hash, Some(peer_b.clone()), 1),
            RelayRecord::New
        );
        // Forward list now includes peer A (provenance reset).
        let fwd = gate.get_forward_peers(&hash, &all_peers);
        assert!(fwd.contains(&peer_a));
        assert!(!fwd.contains(&peer_b));
        assert!(fwd.contains(&peer_c));
    }
}
