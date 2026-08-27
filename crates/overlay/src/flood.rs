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
//!
//! # Design
//!
//! All seen-message state is unified in a single [`EvictingSeenMap`] behind a
//! `Mutex`. This eliminates the split-state complexity of the previous design
//! (separate DashMap + VecDeque + generation tokens). Eviction order is
//! approximate FIFO via `IndexMap` insertion order.

use henyey_common::Hash256;
use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use stellar_xdr::StellarMessage;
use tracing::{debug, trace, warn};

use crate::PeerId;

/// Internal result of recording a message hash in the [`FloodGate`].
///
/// **Module-private by design.** This enum is deliberately NOT exported from
/// the `flood` module. External callers interact via [`FloodGate::record_inbound_relay`]
/// and [`FloodGate::record_local_broadcast`], which return `()` — making it
/// structurally impossible to accidentally use relay status as a drop signal
/// (the c6118f2c / #2317 bug class).
///
/// Actual dedup happens downstream:
/// - SCP: `scp_scheduled_envelopes` in `pump_scp_intake`
/// - Tx: herder `receive_transaction` / tx queue
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayRecord {
    /// First time this hash was seen.
    New,
    /// Hash was already recorded.
    Repeated,
}

/// Default TTL for seen messages (5 minutes).
///
/// Used by [`FloodGate::clear_below`] as a secondary expiry mechanism
/// during ledger-boundary cleanup.
const DEFAULT_TTL_SECS: u64 = 300;

/// Hard cap for the flood seen map (entry count, not memory).
///
/// Bounds the number of tracked message hashes. Under normal mainnet
/// traffic (~1000 unique msgs/sec), the map holds ~5000 entries per
/// ledger close cycle. This cap exists to prevent OOM under adversarial
/// conditions (Sybil flooding with unique hashes).
///
/// Henyey-specific. stellar-core has no equivalent bound — its Floodgate
/// relies solely on `clearBelow()`.
const DEFAULT_MAX_SEEN_ENTRIES: usize = 1_000_000;

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

/// Capacity-bounded seen-message map with approximate-FIFO eviction.
///
/// All state lives in one `IndexMap` (insertion-ordered hash map). Eviction
/// removes the oldest entries by insertion order. `forget()` uses O(1)
/// `swap_remove` which can perturb one survivor's eviction order — this is
/// acceptable because eviction is a memory-safety heuristic, not a
/// correctness mechanism.
struct EvictingSeenMap {
    /// Insertion-ordered entries. Keys are unique message hashes.
    entries: IndexMap<Hash256, SeenEntry>,
    /// Hard capacity bound.
    max_entries: usize,
}

impl EvictingSeenMap {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: IndexMap::new(),
            max_entries,
        }
    }

    /// Evicts oldest entries until the map is at or below 75% capacity.
    /// Returns the number of entries evicted.
    fn evict_to_target(&mut self) -> u64 {
        let target = (self.max_entries * 3 / 4).max(1);
        let current = self.entries.len();
        if current <= target {
            return 0;
        }
        let to_remove = current - target;
        self.entries.drain(..to_remove);
        to_remove as u64
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
/// All operations are thread-safe. Map state is protected by a single Mutex.
/// Lock hold time is O(1) for normal operations (insert, lookup, forget).
/// `clear_below()` holds the lock for O(n) but runs only at ledger close (~5s).
///
/// # Example
///
/// ```rust,ignore
/// let gate = FloodGate::new();
///
/// // Record an inbound message with metric callbacks
/// let hash = compute_message_hash(&message);
/// gate.record_inbound_relay(hash, peer_id, current_ledger_seq,
///     || metrics.unique.inc(),
///     || metrics.duplicate.inc(),
/// );
/// // Message always continues to processing — no drop decision possible.
///
/// // Determine forwarding targets (excludes peers that already sent us this hash)
/// let forward_to = gate.get_forward_peers(&hash, &all_peers);
/// ```
pub struct FloodGate {
    /// Unified seen map — all map state behind a single lock.
    map: Mutex<EvictingSeenMap>,
    /// Time-to-live for message entries (used by `clear_below`).
    ttl: Duration,
    /// Counter: total messages processed.
    messages_seen: AtomicU64,
    /// Counter: duplicate messages observed.
    messages_duplicate: AtomicU64,
    /// Counter: total entries evicted due to capacity overflow.
    evictions_total: AtomicU64,
    /// Maximum messages per second.
    rate_limit: u64,
    /// Start of current rate-limiting window.
    rate_window_start: RwLock<Instant>,
    /// Messages counted in current window.
    rate_window_count: AtomicU64,
    /// Whether an eviction warning has been emitted since last reset.
    eviction_warned: AtomicBool,
}

impl FloodGate {
    /// Creates a new flood gate with default settings (5 minute TTL, 1M entry cap).
    pub fn new() -> Self {
        Self::with_limits(
            Duration::from_secs(DEFAULT_TTL_SECS),
            DEFAULT_MAX_SEEN_ENTRIES,
        )
    }

    /// Creates a new flood gate with a custom TTL and default capacity.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self::with_limits(ttl, DEFAULT_MAX_SEEN_ENTRIES)
    }

    /// Creates a new flood gate with custom TTL and capacity.
    ///
    /// # Panics
    /// Panics if `max_entries == 0`.
    pub fn with_limits(ttl: Duration, max_entries: usize) -> Self {
        assert!(max_entries > 0, "FloodGate max_entries must be > 0");
        Self {
            map: Mutex::new(EvictingSeenMap::new(max_entries)),
            ttl,
            messages_seen: AtomicU64::new(0),
            messages_duplicate: AtomicU64::new(0),
            evictions_total: AtomicU64::new(0),
            rate_limit: DEFAULT_RATE_LIMIT_PER_SEC,
            rate_window_start: RwLock::new(Instant::now()),
            rate_window_count: AtomicU64::new(0),
            eviction_warned: AtomicBool::new(false),
        }
    }

    /// Records that a message has been seen, optionally from a specific peer.
    ///
    /// Returns [`RelayRecord::New`] if this is the first time seeing this
    /// message, or [`RelayRecord::Repeated`] if it was already recorded.
    ///
    /// **Private by design.** External callers use [`record_inbound_relay`] or
    /// [`record_local_broadcast`] which return `()`, preventing accidental use
    /// of relay status as a drop signal (c6118f2c / #2317 bug class).
    ///
    /// This is a pure insert/lookup operation with no automatic cleanup,
    /// matching stellar-core's `addRecord()`. Cleanup happens at ledger
    /// boundaries via [`clear_below`](FloodGate::clear_below).
    fn record_seen(
        &self,
        message_hash: Hash256,
        from_peer: Option<PeerId>,
        ledger_seq: u32,
    ) -> RelayRecord {
        self.messages_seen.fetch_add(1, Ordering::Relaxed);

        let mut map = self.map.lock();
        match map.entries.entry(message_hash) {
            indexmap::map::Entry::Occupied(mut occ) => {
                if let Some(peer) = from_peer {
                    occ.get_mut().add_peer(peer);
                }
                drop(map);
                self.messages_duplicate.fetch_add(1, Ordering::Relaxed);
                trace!("Duplicate message: {}", message_hash);
                RelayRecord::Repeated
            }
            indexmap::map::Entry::Vacant(vac) => {
                let mut entry = SeenEntry::new(ledger_seq);
                if let Some(peer) = from_peer {
                    entry.add_peer(peer);
                }
                vac.insert(entry);
                let evicted = if map.entries.len() > map.max_entries {
                    map.evict_to_target()
                } else {
                    0
                };
                drop(map);
                if evicted > 0 {
                    self.evictions_total.fetch_add(evicted, Ordering::Relaxed);
                    if !self.eviction_warned.swap(true, Ordering::Relaxed) {
                        warn!(
                            evicted,
                            "FloodGate capacity overflow — evicted oldest entries"
                        );
                    }
                }
                trace!("New message: {}", message_hash);
                RelayRecord::New
            }
        }
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

    /// Record a locally-originated message for relay accounting (self-broadcast).
    ///
    /// Returns nothing — the message always continues to processing. This
    /// method is for messages this node originates or re-broadcasts, where
    /// no inbound peer needs recording and no metrics callback is needed.
    ///
    /// stellar-core parity: mirrors the `addRecord(hash, nullptr)` path in
    /// `Floodgate.cpp` for self-originated messages.
    pub(crate) fn record_local_broadcast(&self, message_hash: Hash256, ledger_seq: u32) {
        self.record_seen(message_hash, None, ledger_seq);
    }

    /// Record a flood-tracked message received from a peer and invoke metric callbacks.
    ///
    /// Returns nothing — the message always continues to processing. The
    /// caller never observes new-vs-repeated status directly; only the
    /// provided closures do. This prevents the accidental misuse pattern
    /// from c6118f2c (#2317) where relay status was used as a drop signal.
    ///
    /// # Arguments
    ///
    /// * `on_new` — called if this is the first time seeing this hash
    /// * `on_repeated` — called if the hash was already recorded
    ///
    /// stellar-core parity:
    /// - SCP: Peer.cpp:1667-1673 — recvFloodedMsgID then unconditional recvSCPEnvelope
    /// - Tx: OverlayManagerImpl.cpp:1224-1229 — recvFloodedMsgID then unconditional recvTransaction
    pub(crate) fn record_inbound_relay(
        &self,
        message_hash: Hash256,
        from_peer: PeerId,
        ledger_seq: u32,
        on_new: impl FnOnce(),
        on_repeated: impl FnOnce(),
    ) {
        match self.record_seen(message_hash, Some(from_peer), ledger_seq) {
            RelayRecord::New => on_new(),
            RelayRecord::Repeated => on_repeated(),
        }
    }

    /// Returns the list of peers to forward a message to.
    ///
    /// Excludes any peers that have already sent us this message (tracked
    /// via relay recording).
    pub(crate) fn get_forward_peers(
        &self,
        message_hash: &Hash256,
        all_peers: &[PeerId],
    ) -> Vec<PeerId> {
        let exclude: HashSet<PeerId> = {
            let map = self.map.lock();
            map.entries
                .get(message_hash)
                .map(|entry| entry.peers.iter().cloned().collect())
                .unwrap_or_default()
        };

        all_peers
            .iter()
            .filter(|p| !exclude.contains(*p))
            .cloned()
            .collect()
    }

    /// Returns true if this message has been seen before.
    /// Only available in tests — not part of the public API to prevent
    /// use as a drop-decision signal.
    #[cfg(test)]
    fn has_seen(&self, message_hash: &Hash256) -> bool {
        self.map.lock().entries.contains_key(message_hash)
    }

    /// Removes a previously-seen message from the flood gate, allowing
    /// it to be treated as new on re-delivery.
    ///
    /// Uses O(1) `swap_remove` which may perturb one other entry's eviction
    /// order. This is acceptable because eviction is a memory-safety heuristic,
    /// not a correctness mechanism.
    ///
    /// Mirrors stellar-core's `Floodgate::forgetRecord(Hash const& h)`
    /// (Floodgate.cpp:197-200). Called when a flood-tracked message is
    /// discarded after initial recording — e.g., SCP envelopes rejected
    /// by herder pre-filter or post-verify gate drift.
    pub(crate) fn forget(&self, message_hash: &Hash256) {
        self.map.lock().entries.swap_remove(message_hash);
    }

    /// Returns current statistics about the flood gate.
    pub fn stats(&self) -> FloodGateStats {
        FloodGateStats {
            seen_count: self.map.lock().entries.len(),
            total_messages: self.messages_seen.load(Ordering::Relaxed),
            duplicate_messages: self.messages_duplicate.load(Ordering::Relaxed),
            evictions_total: self.evictions_total.load(Ordering::Relaxed),
        }
    }

    /// Estimate the heap footprint of the seen-message map (#3845).
    ///
    /// O(1): reads the entry count only. The per-entry `peers` sets are *not*
    /// walked — that would be O(n) over up to `max_entries` (~1M) entries on
    /// the ledger-close report path — so only the flat `IndexMap` payload (the
    /// dominant term) is counted. Returns `(bytes, entry_count)`.
    pub fn estimate_heap_bytes(&self) -> (u64, u64) {
        use henyey_common::memory::hashmap_heap_bytes;
        let len = self.map.lock().entries.len();
        // IndexMap ≈ a Vec of (K, V) entries plus a hash index table; the flat
        // hashmap helper over the entry count approximates both.
        let bytes = hashmap_heap_bytes(
            len,
            std::mem::size_of::<Hash256>(),
            std::mem::size_of::<SeenEntry>(),
        );
        (bytes as u64, len as u64)
    }

    /// Removes flood records from ledgers before `ledger_seq`.
    ///
    /// Matches upstream stellar-core's `clearBelow(maxLedger)` which removes
    /// records from ledgers before `maxLedger`. Additionally removes
    /// TTL-expired entries as a henyey-specific defensive measure (stellar-core's
    /// `clearBelow` is purely ledger-based).
    pub fn clear_below(&self, ledger_seq: u32) {
        let ttl = self.ttl;
        let mut map = self.map.lock();
        let before_count = map.entries.len();
        map.entries
            .retain(|_, entry| entry.ledger_seq >= ledger_seq && !entry.is_expired(ttl));
        let after_count = map.entries.len();
        let max_entries = map.max_entries;
        drop(map);

        let removed = before_count.saturating_sub(after_count);
        if removed > 0 {
            debug!(
                "FloodGate clear_below({}): removed {} entries",
                ledger_seq, removed
            );
        }

        // Reset eviction warning if map dropped well below capacity
        if after_count < max_entries / 2 {
            self.eviction_warned.store(false, Ordering::Relaxed);
        }
    }

    /// Clears all entries from the flood gate.
    ///
    /// Use with caution - this will allow previously-seen messages to be
    /// flooded again.
    pub fn clear(&self) {
        self.map.lock().entries.clear();
        self.eviction_warned.store(false, Ordering::Relaxed);
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
    /// Total entries evicted due to capacity overflow.
    pub evictions_total: u64,
}

/// Computes the BLAKE2b-256 hash of a message for flood tracking.
///
/// This matches stellar-core's `xdrBlake2()` used in `Floodgate::broadcast()`.
pub fn compute_message_hash(message: &StellarMessage) -> Hash256 {
    use stellar_xdr::{Limits, WriteXdr};
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

    /// OVERLAY_SPEC.md §3.3: flood message deduplication MUST use BLAKE2b-256
    /// over the XDR serialization of the entire `StellarMessage` (matching
    /// stellar-core's `xdrBlake2()`), NOT SHA-256. This guards against a
    /// regression to SHA-256 — the dedup hash is local-only, so a divergence
    /// would silently waste CPU without a wire-visible failure.
    #[test]
    fn test_flood_dedup_hash_is_blake2b_not_sha256() {
        use stellar_xdr::{Limits, WriteXdr};

        let message = StellarMessage::Peers(Default::default());
        let bytes = message.to_xdr(Limits::none()).unwrap();

        // The flood-dedup hash must equal BLAKE2b-256 of the XDR bytes.
        let expected_blake2 = henyey_crypto::blake2(&bytes);
        assert_eq!(compute_message_hash(&message), expected_blake2);

        // ...and must NOT equal SHA-256 of the same bytes, which is the
        // wrong primitive the spec audit (#3081) flagged.
        let sha256 = henyey_common::Hash256::hash(&bytes);
        assert_ne!(
            compute_message_hash(&message),
            sha256,
            "flood dedup hash must be BLAKE2b-256, not SHA-256"
        );
    }

    #[test]
    fn test_flood_gate_basic() {
        let gate = FloodGate::new();

        let hash = make_hash(1);
        assert!(!gate.has_seen(&hash));

        // Record as seen
        assert_eq!(gate.record_seen(hash, None, 1), RelayRecord::New);

        // Should be seen now
        assert!(gate.has_seen(&hash));

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

        // Should not be seen anymore
        assert!(!gate.has_seen(&hash));
    }

    #[test]
    fn test_flood_record() {
        let message = StellarMessage::Peers(stellar_xdr::VecM::default());
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

        // Pull-control messages are flood messages but NOT gate-tracked
        let advert = StellarMessage::FloodAdvert(Default::default());
        let demand = StellarMessage::FloodDemand(Default::default());
        let tx =
            StellarMessage::Transaction(stellar_xdr::TransactionEnvelope::TxV0(Default::default()));

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
        assert_eq!(gate.map.lock().entries.len(), 0);

        // Transaction IS gate-tracked and should be recorded
        assert!(helpers::is_flood_gate_tracked(&tx));
        if helpers::is_flood_gate_tracked(&tx) {
            let _ = gate.record_seen(compute_message_hash(&tx), None, 1);
        }
        assert_eq!(gate.map.lock().entries.len(), 1);
    }

    #[test]
    fn test_flood_gate_forget_basic() {
        let gate = FloodGate::new();
        let hash = make_hash(1);

        // Record, then forget — has_seen returns false again.
        assert_eq!(gate.record_seen(hash, None, 1), RelayRecord::New);
        assert!(gate.has_seen(&hash));

        gate.forget(&hash);
        assert!(!gate.has_seen(&hash));
    }

    #[test]
    fn test_flood_gate_forget_nonexistent() {
        let gate = FloodGate::new();
        let hash = make_hash(42);

        // Forgetting a hash that was never recorded is a no-op.
        gate.forget(&hash);
        assert!(!gate.has_seen(&hash));
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

    #[test]
    fn test_record_local_broadcast() {
        let gate = FloodGate::new();
        let hash = make_hash(1);

        assert!(!gate.has_seen(&hash));
        gate.record_local_broadcast(hash, 1);
        assert!(gate.has_seen(&hash));

        // Calling again doesn't panic, just increments duplicate counter
        gate.record_local_broadcast(hash, 1);
        assert_eq!(gate.stats().duplicate_messages, 1);
    }

    #[test]
    fn test_record_inbound_relay_calls_on_new() {
        let gate = FloodGate::new();
        let hash = make_hash(1);
        let peer = make_peer_id(1);

        let mut new_called = false;
        let mut repeated_called = false;

        gate.record_inbound_relay(
            hash,
            peer,
            1,
            || new_called = true,
            || repeated_called = true,
        );

        assert!(new_called);
        assert!(!repeated_called);
        assert!(gate.has_seen(&hash));
    }

    #[test]
    fn test_record_inbound_relay_calls_on_repeated() {
        let gate = FloodGate::new();
        let hash = make_hash(1);
        let peer1 = make_peer_id(1);
        let peer2 = make_peer_id(2);

        // First record
        gate.record_inbound_relay(hash, peer1, 1, || {}, || {});

        // Second record from different peer
        let mut new_called = false;
        let mut repeated_called = false;
        gate.record_inbound_relay(
            hash,
            peer2,
            1,
            || new_called = true,
            || repeated_called = true,
        );

        assert!(!new_called);
        assert!(repeated_called);
    }

    #[test]
    fn test_record_inbound_relay_accumulates_peers() {
        let gate = FloodGate::new();
        let hash = make_hash(1);
        let peer1 = make_peer_id(1);
        let peer2 = make_peer_id(2);
        let peer3 = make_peer_id(3);

        gate.record_inbound_relay(hash, peer1.clone(), 1, || {}, || {});
        gate.record_inbound_relay(hash, peer2.clone(), 1, || {}, || {});

        // get_forward_peers should exclude peer1 and peer2
        let all_peers = vec![peer1.clone(), peer2.clone(), peer3.clone()];
        let forward = gate.get_forward_peers(&hash, &all_peers);
        assert_eq!(forward, vec![peer3]);
    }

    // --- Capacity eviction tests ---

    #[test]
    fn test_flood_gate_eviction_at_capacity() {
        let gate = FloodGate::with_limits(Duration::from_secs(300), 100);

        // Fill to capacity
        for i in 0..100u8 {
            let _ = gate.record_seen(make_hash(i), None, 1);
        }
        assert_eq!(gate.stats().seen_count, 100);

        // One more triggers eviction to 75% target
        let _ = gate.record_seen(make_hash(200), None, 1);
        assert!(gate.stats().seen_count <= 75);
        assert!(gate.stats().evictions_total > 0);
    }

    #[test]
    fn test_flood_gate_fifo_order() {
        let gate = FloodGate::with_limits(Duration::from_secs(300), 4);

        let ha = make_hash(1);
        let hb = make_hash(2);
        let hc = make_hash(3);
        let hd = make_hash(4);

        let _ = gate.record_seen(ha, None, 1);
        let _ = gate.record_seen(hb, None, 1);
        let _ = gate.record_seen(hc, None, 1);
        let _ = gate.record_seen(hd, None, 1);

        // Insert one more — triggers eviction. Oldest entries should be evicted.
        let he = make_hash(5);
        let _ = gate.record_seen(he, None, 1);

        // Target is max(4*3/4, 1) = 3. So we evict until <= 3.
        // After inserting 5th, we had 5 entries, evict oldest until <= 3.
        assert!(!gate.has_seen(&ha)); // A evicted (oldest)
        assert!(!gate.has_seen(&hb)); // B evicted
        assert!(gate.has_seen(&he)); // E (newest) survives
    }

    #[test]
    fn test_clear_below_then_eviction() {
        // Previously: test_flood_gate_ghost_entries_skipped
        // In the unified design, clear_below directly removes entries.
        // Subsequent eviction operates only on live entries.
        let gate = FloodGate::with_limits(Duration::from_secs(300), 10);

        // Insert entries at ledger 1 and ledger 2
        for i in 0..5u8 {
            let _ = gate.record_seen(make_hash(i), None, 1);
        }
        for i in 10..15u8 {
            let _ = gate.record_seen(make_hash(i), None, 2);
        }
        assert_eq!(gate.stats().seen_count, 10);

        // clear_below(2) removes ledger-1 entries directly
        gate.clear_below(2);
        assert_eq!(gate.stats().seen_count, 5);

        // Now fill up to trigger eviction. Only live entries are considered.
        for i in 20..30u8 {
            let _ = gate.record_seen(make_hash(i), None, 2);
        }
        // Should have triggered eviction; verify we're at or below cap
        assert!(gate.stats().seen_count <= 10);
    }

    #[test]
    fn test_forget_then_eviction() {
        // Previously: test_flood_gate_forget_creates_ghost
        // In the unified design, forget removes the entry completely.
        let gate = FloodGate::with_limits(Duration::from_secs(300), 4);

        let ha = make_hash(1);
        let hb = make_hash(2);
        let hc = make_hash(3);
        let hd = make_hash(4);

        let _ = gate.record_seen(ha, None, 1);
        let _ = gate.record_seen(hb, None, 1);
        let _ = gate.record_seen(hc, None, 1);
        let _ = gate.record_seen(hd, None, 1);

        // Forget B — directly removes it (no ghost)
        gate.forget(&hb);
        assert_eq!(gate.stats().seen_count, 3);

        // Insert two more to trigger eviction
        let he = make_hash(5);
        let hf = make_hash(6);
        let _ = gate.record_seen(he, None, 1);
        // Now 4 entries (A, C, D, E) — at capacity
        let _ = gate.record_seen(hf, None, 1);
        // Now 5 entries, eviction triggered. Target = max(3, 1) = 3.
        // Oldest entries evicted first.
        assert!(gate.stats().seen_count <= 3);
        assert!(gate.has_seen(&hf)); // newest survives
    }

    #[test]
    fn test_forget_reinsert_eviction_order() {
        // Previously: test_flood_gate_generation_prevents_stale_eviction
        // In the unified design, forget+reinsert places the entry at the
        // back of insertion order (newest position).
        let gate = FloodGate::with_limits(Duration::from_secs(300), 4);

        let ha = make_hash(1);
        let hb = make_hash(2);
        let hc = make_hash(3);
        let hd = make_hash(4);

        let _ = gate.record_seen(ha, None, 1);
        let _ = gate.record_seen(hb, None, 1);
        let _ = gate.record_seen(hc, None, 1);
        let _ = gate.record_seen(hd, None, 1);

        // Forget A, then re-insert A (goes to back of insertion order)
        gate.forget(&ha);
        let _ = gate.record_seen(ha, None, 2);
        // Map: B, C, D, A (in insertion order, with A at back due to reinsert)
        // Note: swap_remove may have moved D to B's former position,
        // but A is definitely at the back.

        // Insert one more to trigger eviction
        let he = make_hash(5);
        let _ = gate.record_seen(he, None, 2);
        // 5 entries > cap of 4. Target = 3. Evict 2 oldest.
        // A should survive because it's near the back.
        assert!(gate.has_seen(&ha)); // Re-inserted A survives
        assert!(gate.has_seen(&he)); // Newest survives
    }

    #[test]
    fn test_clear_resets_map() {
        // Previously: test_flood_gate_clear_resets_queue
        let gate = FloodGate::with_limits(Duration::from_secs(300), 100);

        for i in 0..50u8 {
            let _ = gate.record_seen(make_hash(i), None, 1);
        }
        assert_eq!(gate.stats().seen_count, 50);

        gate.clear();
        assert_eq!(gate.stats().seen_count, 0);
        assert!(gate.map.lock().entries.is_empty());
    }

    #[test]
    fn test_flood_gate_eviction_stats() {
        let gate = FloodGate::with_limits(Duration::from_secs(300), 10);

        // Fill to capacity
        for i in 0..10u8 {
            let _ = gate.record_seen(make_hash(i), None, 1);
        }
        assert_eq!(gate.stats().evictions_total, 0);

        // Trigger eviction
        let _ = gate.record_seen(make_hash(100), None, 1);
        assert!(gate.stats().evictions_total > 0);
    }

    #[test]
    fn test_flood_gate_no_eviction_below_cap() {
        let gate = FloodGate::with_limits(Duration::from_secs(300), 100);

        for i in 0..50u8 {
            let _ = gate.record_seen(make_hash(i), None, 1);
        }
        assert_eq!(gate.stats().seen_count, 50);
        assert_eq!(gate.stats().evictions_total, 0);
    }

    #[test]
    #[should_panic(expected = "FloodGate max_entries must be > 0")]
    fn test_flood_gate_with_limits_panics_on_zero() {
        FloodGate::with_limits(Duration::from_secs(300), 0);
    }

    #[test]
    fn test_flood_gate_eviction_warn_resets() {
        let gate = FloodGate::with_limits(Duration::from_secs(300), 10);

        // Fill and trigger eviction → sets eviction_warned
        for i in 0..11u8 {
            let _ = gate.record_seen(make_hash(i), None, 1);
        }
        assert!(gate.eviction_warned.load(Ordering::Relaxed));

        // clear_below that brings map below 50% should reset the warning
        gate.clear_below(u32::MAX); // removes everything (ledger < MAX)
        assert!(!gate.eviction_warned.load(Ordering::Relaxed));
    }

    #[test]
    fn test_flood_gate_concurrent_bounded() {
        use std::sync::Arc;
        use std::thread;

        let gate = Arc::new(FloodGate::with_limits(Duration::from_secs(300), 1000));
        let num_threads = 8;
        let inserts_per_thread = 500;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    for i in 0..inserts_per_thread {
                        let hash = Hash256([(t * 100 + i) as u8; 32]);
                        let _ = gate.record_seen(hash, None, 1);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // After all threads complete, map should be at or below cap
        // (with Mutex, no overshoot is possible unlike DashMap)
        assert!(
            gate.stats().seen_count <= 1000,
            "seen_count = {} exceeds cap",
            gate.stats().seen_count
        );
    }

    /// Regression test: concurrent forget+reinsert must not lose entries.
    /// With the unified Mutex design, atomicity is inherent — the mutex
    /// serializes all operations, preventing the TOCTOU races that the
    /// generation-token design guarded against.
    #[test]
    fn test_flood_gate_concurrent_forget_reinsert_during_eviction() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};
        use std::thread;

        const NUM_STUFFER_THREADS: usize = 3;
        const STUFFER_ITERATIONS: usize = 300;

        let gate = Arc::new(FloodGate::with_limits(Duration::from_secs(300), 50));

        // Pre-fill to capacity.
        for i in 0u16..50 {
            let mut h = [0u8; 32];
            h[0..2].copy_from_slice(&i.to_le_bytes());
            gate.record_seen(Hash256(h), None, 1);
        }

        let hot_hash = Hash256([0xAA; 32]);
        gate.record_seen(hot_hash, None, 1);

        let stuffers_remaining = Arc::new(AtomicUsize::new(NUM_STUFFER_THREADS));
        let barrier = Arc::new(Barrier::new(NUM_STUFFER_THREADS + 1));

        let mut handles = Vec::new();

        // Spawn stuffer threads.
        for t in 0..NUM_STUFFER_THREADS {
            let gate = Arc::clone(&gate);
            let stuffers_remaining = Arc::clone(&stuffers_remaining);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                for i in 0..STUFFER_ITERATIONS {
                    let mut h = [0u8; 32];
                    h[0] = 0xFF;
                    h[1] = t as u8;
                    h[2..4].copy_from_slice(&(i as u16).to_le_bytes());
                    gate.record_seen(Hash256(h), None, 1);
                }
                stuffers_remaining.fetch_sub(1, Ordering::Release);
                barrier.wait();
            }));
        }

        // Spawn single cycler thread.
        {
            let gate = Arc::clone(&gate);
            let stuffers_remaining = Arc::clone(&stuffers_remaining);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                while stuffers_remaining.load(Ordering::Acquire) > 0 {
                    gate.forget(&hot_hash);
                    gate.record_seen(hot_hash, None, 1);
                }
                // Final guaranteed reinsert after all stuffers done.
                gate.forget(&hot_hash);
                gate.record_seen(hot_hash, None, 1);
                barrier.wait();
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // The hot hash must still be present — final reinsert guarantees it.
        assert!(
            gate.has_seen(&hot_hash),
            "hot_hash was incorrectly lost during concurrent operations"
        );
    }

    /// Invariant test: two threads racing to `record_seen` the same hash
    /// (after a forget) must produce exactly one `New` and one `Repeated`.
    /// With the Mutex design, this is guaranteed by serialization.
    #[test]
    fn test_record_seen_concurrent_forget_reinsert_is_atomic() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let gate = Arc::new(FloodGate::with_limits(Duration::from_secs(300), 200));
        let hash = Hash256([0xBB; 32]);

        // Pre-fill with 180 entries (below capacity, no eviction yet).
        for i in 0u16..180 {
            let mut h = [0u8; 32];
            h[0..2].copy_from_slice(&i.to_le_bytes());
            gate.record_seen(Hash256(h), None, 1);
        }

        // Insert hash, then forget it.
        gate.record_seen(hash, None, 1);
        gate.forget(&hash);

        // Two threads race to record_seen the same hash.
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let gate = Arc::clone(&gate);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    gate.record_seen(hash, None, 1)
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Exactly one New, one Repeated (serialization invariant).
        let new_count = results.iter().filter(|r| **r == RelayRecord::New).count();
        let repeated_count = results
            .iter()
            .filter(|r| **r == RelayRecord::Repeated)
            .count();
        assert_eq!(new_count, 1, "exactly one thread should see New");
        assert_eq!(repeated_count, 1, "exactly one thread should see Repeated");

        // The hash must be present after both threads complete.
        assert!(gate.has_seen(&hash));
    }

    // --- New tests for boundary behavior and order perturbation ---

    #[test]
    fn test_eviction_target_floor_max1() {
        let gate = FloodGate::with_limits(Duration::from_secs(300), 1);

        let _ = gate.record_seen(make_hash(1), None, 1);
        assert_eq!(gate.stats().seen_count, 1);

        // Insert second entry — triggers eviction. Target = max(0, 1) = 1.
        // Should evict 1, leaving 1 entry (the newest).
        let _ = gate.record_seen(make_hash(2), None, 1);
        assert_eq!(gate.stats().seen_count, 1);
        assert!(gate.has_seen(&make_hash(2)));
        assert!(!gate.has_seen(&make_hash(1)));
    }

    #[test]
    fn test_eviction_target_floor_max2() {
        let gate = FloodGate::with_limits(Duration::from_secs(300), 2);

        let _ = gate.record_seen(make_hash(1), None, 1);
        let _ = gate.record_seen(make_hash(2), None, 1);
        assert_eq!(gate.stats().seen_count, 2);

        // Insert third — triggers eviction. Target = max(1, 1) = 1.
        let _ = gate.record_seen(make_hash(3), None, 1);
        assert!(gate.stats().seen_count <= 2);
        assert!(gate.has_seen(&make_hash(3))); // newest survives
    }

    #[test]
    fn test_eviction_target_floor_max3() {
        let gate = FloodGate::with_limits(Duration::from_secs(300), 3);

        for i in 1..=3u8 {
            let _ = gate.record_seen(make_hash(i), None, 1);
        }
        assert_eq!(gate.stats().seen_count, 3);

        // Insert fourth — triggers eviction. Target = max(2, 1) = 2.
        let _ = gate.record_seen(make_hash(4), None, 1);
        assert!(gate.stats().seen_count <= 3);
        assert!(gate.has_seen(&make_hash(4))); // newest survives
    }

    #[test]
    fn test_forget_does_not_leak() {
        let gate = FloodGate::with_limits(Duration::from_secs(300), 100);

        // Insert N entries, then forget all of them.
        for i in 0..50u8 {
            let _ = gate.record_seen(make_hash(i), None, 1);
        }
        assert_eq!(gate.stats().seen_count, 50);

        for i in 0..50u8 {
            gate.forget(&make_hash(i));
        }
        assert_eq!(gate.stats().seen_count, 0);
    }

    #[test]
    fn test_swap_remove_order_perturbation() {
        // Verify that forget() using swap_remove can perturb eviction order.
        // This documents the intentional behavior.
        let gate = FloodGate::with_limits(Duration::from_secs(300), 10);

        // Insert A, B, C, D, E in order
        let ha = make_hash(1);
        let hb = make_hash(2);
        let hc = make_hash(3);
        let hd = make_hash(4);
        let he = make_hash(5);

        let _ = gate.record_seen(ha, None, 1);
        let _ = gate.record_seen(hb, None, 1);
        let _ = gate.record_seen(hc, None, 1);
        let _ = gate.record_seen(hd, None, 1);
        let _ = gate.record_seen(he, None, 1);

        // Forget B — swap_remove moves E (last) into B's position.
        gate.forget(&hb);

        // Verify the map still has A, C, D, E
        assert!(gate.has_seen(&ha));
        assert!(!gate.has_seen(&hb));
        assert!(gate.has_seen(&hc));
        assert!(gate.has_seen(&hd));
        assert!(gate.has_seen(&he));
        assert_eq!(gate.stats().seen_count, 4);

        // Verify order: E is now at position 1 (B's old position), so
        // eviction order is now [A, E, C, D] instead of [A, C, D, E].
        // This means E would be evicted before C and D — documenting
        // the intentional approximate-FIFO behavior.
        let map = gate.map.lock();
        let keys: Vec<_> = map.entries.keys().cloned().collect();
        // E should be at index 1 (moved from index 4 to index 1 by swap_remove)
        assert_eq!(keys[0], ha);
        assert_eq!(keys[1], he); // E moved to B's position
        assert_eq!(keys[2], hc);
        assert_eq!(keys[3], hd);
    }

    /// #3845: `estimate_heap_bytes` is 0 on an empty flood gate and grows
    /// monotonically as messages are recorded.
    #[test]
    fn test_flood_gate_estimate_heap_bytes_monotonic() {
        let gate = FloodGate::new();
        let (bytes0, count0) = gate.estimate_heap_bytes();
        assert_eq!((bytes0, count0), (0, 0), "empty flood gate costs 0");

        gate.record_local_broadcast(make_hash(1), 1);
        let (bytes1, count1) = gate.estimate_heap_bytes();
        assert!(bytes1 > 0);
        assert_eq!(count1, 1);

        gate.record_local_broadcast(make_hash(2), 1);
        let (bytes2, count2) = gate.estimate_heap_bytes();
        assert!(
            bytes2 > bytes1,
            "a second message must increase the estimate"
        );
        assert_eq!(count2, 2);
    }
}
