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
//! - **TTL-based Expiry**: Old entries are automatically cleaned up after
//!   a configurable TTL (default 5 minutes) to prevent unbounded memory growth.
//!
//! - **Rate Limiting**: Soft limit on messages per second to prevent
//!   overwhelming the node during traffic spikes.

use dashmap::DashMap;
use henyey_common::Hash256;
use henyey_common::LedgerSeq;
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use stellar_xdr::curr::StellarMessage;
use tracing::{debug, trace};

use crate::PeerId;

/// Default TTL for seen messages (5 minutes).
///
/// Messages older than this are forgotten, allowing them to be flooded again
/// if re-received (which shouldn't normally happen).
const DEFAULT_TTL_SECS: u64 = 300;

/// Maximum entries before forced cleanup.
///
/// Prevents unbounded memory growth under heavy traffic.
const MAX_ENTRIES: usize = 100_000;

/// How often to check for expired entries (1 minute).
const CLEANUP_INTERVAL_SECS: u64 = 60;

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
    ledger_seq: LedgerSeq,
    /// Set of peers that have sent us this message.
    peers: HashSet<PeerId>,
}

impl SeenEntry {
    /// Creates a new entry with the current timestamp and ledger sequence.
    fn new(ledger_seq: LedgerSeq) -> Self {
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
/// // Check if we should flood a message
/// let hash = compute_message_hash(&message);
/// if gate.record_seen(hash, Some(peer_id), current_ledger_seq) {
///     // First time seeing this - flood to other peers
///     let forward_to = gate.get_forward_peers(&hash, &all_peers);
/// }
/// ```
pub struct FloodGate {
    /// Map of message hash to tracking entry.
    seen: DashMap<Hash256, SeenEntry>,
    /// Time-to-live for message entries.
    ttl: Duration,
    /// Last time we ran cleanup.
    last_cleanup: RwLock<Instant>,
    /// Counter: total messages processed.
    messages_seen: AtomicU64,
    /// Counter: duplicate messages dropped.
    messages_dropped: AtomicU64,
    /// Maximum messages per second.
    rate_limit: u64,
    /// Start of current rate-limiting window.
    rate_window_start: RwLock<Instant>,
    /// Messages counted in current window.
    rate_window_count: AtomicU64,
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
            last_cleanup: RwLock::new(Instant::now()),
            messages_seen: AtomicU64::new(0),
            messages_dropped: AtomicU64::new(0),
            rate_limit: DEFAULT_RATE_LIMIT_PER_SEC,
            rate_window_start: RwLock::new(Instant::now()),
            rate_window_count: AtomicU64::new(0),
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
    /// Returns `true` if this is the first time seeing this message (should flood),
    /// or `false` if it's a duplicate (should drop).
    ///
    /// If `from_peer` is `Some`, that peer is recorded so we don't forward
    /// the message back to them.
    ///
    /// The `ledger_seq` parameter records the current ledger sequence for
    /// ledger-based cleanup via [`clear_below`](FloodGate::clear_below).
    pub fn record_seen(
        &self,
        message_hash: Hash256,
        from_peer: Option<PeerId>,
        ledger_seq: LedgerSeq,
    ) -> bool {
        self.messages_seen.fetch_add(1, Ordering::Relaxed);

        // Try cleanup if needed
        self.maybe_cleanup();

        // Check if we've seen this message
        if let Some(mut entry) = self.seen.get_mut(&message_hash) {
            // Already seen, record the peer
            if let Some(peer) = from_peer {
                entry.add_peer(peer);
            }
            self.messages_dropped.fetch_add(1, Ordering::Relaxed);
            trace!("Duplicate message: {}", message_hash);
            return false;
        }

        // New message
        let mut entry = SeenEntry::new(ledger_seq);
        if let Some(peer) = from_peer {
            entry.add_peer(peer);
        }
        self.seen.insert(message_hash, entry);
        trace!("New message: {}", message_hash);
        true
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

    /// Forces immediate cleanup of expired entries.
    ///
    /// Normally cleanup happens automatically, but this can be called
    /// to free memory immediately.
    pub fn cleanup(&self) {
        let now = Instant::now();
        let ttl = self.ttl;

        let before_count = self.seen.len();
        self.seen.retain(|_, entry| !entry.is_expired(ttl));
        // Use saturating_sub to avoid panic if entries were added by another thread
        // between the two len() calls
        let removed = before_count.saturating_sub(self.seen.len());

        if removed > 0 {
            debug!("FloodGate cleanup: removed {} expired entries", removed);
        }

        *self.last_cleanup.write() = now;
    }

    /// Runs cleanup if the interval has passed or we've exceeded max entries.
    fn maybe_cleanup(&self) {
        let should_cleanup = {
            let last = *self.last_cleanup.read();
            last.elapsed() > Duration::from_secs(CLEANUP_INTERVAL_SECS)
                || self.seen.len() > MAX_ENTRIES
        };

        if should_cleanup {
            self.cleanup();
        }
    }

    /// Returns current statistics about the flood gate.
    pub fn stats(&self) -> FloodGateStats {
        FloodGateStats {
            seen_count: self.seen.len(),
            total_messages: self.messages_seen.load(Ordering::Relaxed),
            dropped_messages: self.messages_dropped.load(Ordering::Relaxed),
        }
    }

    /// Removes flood records from ledgers before `ledger_seq`.
    ///
    /// Matches upstream stellar-core's `clearBelow(maxLedger)` which removes
    /// records from ledgers before `maxLedger`. Also runs time-based TTL
    /// cleanup as a secondary mechanism.
    pub fn clear_below(&self, ledger_seq: LedgerSeq) {
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

        *self.last_cleanup.write() = Instant::now();
    }

    /// Clears all entries from the flood gate.
    ///
    /// Use with caution - this will allow previously-seen messages to be
    /// flooded again.
    pub fn clear(&self) {
        self.seen.clear();
        *self.last_cleanup.write() = Instant::now();
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
    /// Number of messages dropped as duplicates.
    pub dropped_messages: u64,
}

impl FloodGateStats {
    /// Calculates the duplicate rate as a percentage.
    ///
    /// Returns 0.0 if no messages have been processed.
    pub fn duplicate_rate(&self) -> f64 {
        if self.total_messages == 0 {
            0.0
        } else {
            (self.dropped_messages as f64 / self.total_messages as f64) * 100.0
        }
    }
}

/// Computes the BLAKE2b-256 hash of a message for flood tracking.
///
/// This matches stellar-core's `xdrBlake2()` used in `Floodgate::broadcast()`.
pub fn compute_message_hash(message: &StellarMessage) -> Hash256 {
    use stellar_xdr::curr::{Limits, WriteXdr};
    let bytes = message.to_xdr(Limits::none()).unwrap_or_default();
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
        assert!(gate.record_seen(hash, None, 1.into()));

        // Should not flood again
        assert!(!gate.should_flood(&hash));

        // Record again should return false
        assert!(!gate.record_seen(hash, None, 1.into()));
    }

    #[test]
    fn test_flood_gate_with_peers() {
        let gate = FloodGate::new();

        let hash = make_hash(1);
        let peer1 = make_peer_id(1);
        let peer2 = make_peer_id(2);
        let peer3 = make_peer_id(3);

        // First seen from peer1
        assert!(gate.record_seen(hash, Some(peer1.clone()), 1.into()));

        // Also seen from peer2
        assert!(!gate.record_seen(hash, Some(peer2.clone()), 1.into()));

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

        gate.record_seen(hash1, None, 1.into());
        gate.record_seen(hash1, None, 1.into()); // duplicate
        gate.record_seen(hash2, None, 1.into());

        let stats = gate.stats();
        assert_eq!(stats.seen_count, 2);
        assert_eq!(stats.total_messages, 3);
        assert_eq!(stats.dropped_messages, 1);
    }

    #[test]
    fn test_flood_gate_expiry() {
        let gate = FloodGate::with_ttl(Duration::from_millis(10));

        let hash = make_hash(1);
        gate.record_seen(hash, None, 1.into());

        // Wait for expiry
        std::thread::sleep(Duration::from_millis(20));

        // Force cleanup
        gate.cleanup();

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
        gate.record_seen(hash1, None, 50.into());
        gate.record_seen(hash2, None, 100.into());
        gate.record_seen(hash3, None, 150.into());

        assert_eq!(gate.stats().seen_count, 3);

        // clear_below(100) removes entries from ledgers < 100
        gate.clear_below(100.into());
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
        gate.record_seen(hash1, None, 100.into());
        gate.record_seen(hash2, None, 100.into());

        assert_eq!(gate.stats().seen_count, 2);

        // Wait for entries to expire
        std::thread::sleep(Duration::from_millis(20));

        // clear_below triggers cleanup of expired entries (even at same ledger)
        gate.clear_below(100.into());

        assert_eq!(gate.stats().seen_count, 0);
    }

    #[test]
    fn test_clear_below_preserves_recent() {
        // Use a long TTL so entries don't expire
        let gate = FloodGate::with_ttl(Duration::from_secs(300));

        let hash1 = make_hash(1);
        let hash2 = make_hash(2);
        gate.record_seen(hash1, None, 100.into());
        gate.record_seen(hash2, None, 100.into());

        // clear_below should not remove entries at or above the threshold
        gate.clear_below(100.into());

        assert_eq!(gate.stats().seen_count, 2);
        assert!(gate.has_seen(&hash1));
        assert!(gate.has_seen(&hash2));
    }
}
