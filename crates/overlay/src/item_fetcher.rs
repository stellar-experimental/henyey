//! Item fetcher for TxSet and QuorumSet retrieval.
//!
//! This module implements the ItemFetcher and Tracker classes from stellar-core.
//! It manages asking peers for Transaction Sets and Quorum Sets during SCP consensus.
//!
//! # Overview
//!
//! - [`ItemFetcher`] manages multiple [`Tracker`] instances, one per item being fetched
//! - [`Tracker`] handles the state machine for fetching a single item from peers
//! - Items are identified by their SHA-256 hash
//! - A callback mechanism allows integration with the overlay manager for sending requests
//!
//! # Protocol
//!
//! 1. When an SCP envelope references an unknown TxSet or QuorumSet, we fetch it
//! 2. The tracker asks peers one at a time with a timeout
//! 3. If a peer responds with DONT_HAVE, we try the next peer
//! 4. If we exhaust all peers, we restart with exponential backoff
//! 5. When the item is received, all waiting envelopes are re-processed
//!
//! # Callback Integration
//!
//! The ItemFetcher supports a callback mechanism for requesting items from peers:
//!
//! 1. Configure the callback with [`ItemFetcher::set_ask_peer`]
//! 2. Update available peers with [`ItemFetcher::set_available_peers`]
//! 3. When [`ItemFetcher::fetch`] is called, the callback is invoked immediately
//! 4. Call [`ItemFetcher::process_pending`] periodically to handle timeouts and retries
//!
//! # Example
//!
//! ```ignore
//! use henyey_overlay::{ItemFetcher, ItemFetcherConfig, ItemType};
//!
//! let mut fetcher = ItemFetcher::new(ItemType::TxSet, ItemFetcherConfig::default(), None);
//!
//! // Set up callback to request items from peers
//! fetcher.set_ask_peer(Box::new(|peer_id, hash, item_type| {
//!     overlay_manager.send_get_tx_set(peer_id, hash);
//! }));
//!
//! // Update available peers
//! fetcher.set_available_peers(overlay_manager.connected_peers());
//!
//! // Start fetching - callback is invoked immediately
//! fetcher.fetch(tx_set_hash, &envelope);
//!
//! // Periodically process timeouts (e.g., every second)
//! fetcher.process_pending();
//! ```

use crate::metrics::OverlayMetrics;
use crate::PeerId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use stellar_xdr::curr::{Hash, ScpEnvelope};
use tracing::{debug, trace};

/// Type of item being fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemType {
    /// Transaction set.
    TxSet,
    /// SCP quorum set.
    QuorumSet,
}

/// Callback type for asking a peer for an item.
/// Parameters: peer_id, item_hash, item_type
pub type AskPeerFn = Box<dyn Fn(&PeerId, &Hash, ItemType) + Send + Sync>;

/// Configuration for item fetching.
#[derive(Debug, Clone)]
pub struct ItemFetcherConfig {
    /// Timeout for waiting for a peer's response.
    pub fetch_reply_timeout: Duration,
    /// Maximum number of times to rebuild the peer list.
    pub max_rebuild_fetch_list: u32,
}

impl Default for ItemFetcherConfig {
    fn default() -> Self {
        Self {
            // Match stellar-core default: 1500ms
            fetch_reply_timeout: Duration::from_millis(1500),
            max_rebuild_fetch_list: 10,
        }
    }
}

/// A tracker for a single item being fetched.
///
/// Manages the state machine for fetching an item from peers:
/// - Tracks which peers have been asked
/// - Handles timeouts and retries
/// - Stores envelopes waiting for this item
pub struct Tracker {
    /// Hash of the item being tracked.
    item_hash: Hash,
    /// Configuration.
    config: ItemFetcherConfig,
    /// Peers that have been asked (and whether they claimed to have the data).
    peers_asked: HashMap<PeerId, bool>,
    /// Last peer we asked.
    last_asked_peer: Option<PeerId>,
    /// Envelopes waiting for this item: (envelope_hash, envelope).
    waiting_envelopes: Vec<(Hash, ScpEnvelope)>,
    /// When we started fetching.
    fetch_start: Instant,
    /// When we last asked a peer.
    last_ask_time: Option<Instant>,
    /// Number of times we've rebuilt the peer list.
    num_list_rebuild: u32,
    /// Biggest slot index seen.
    last_seen_slot_index: u64,
}

impl Tracker {
    /// Create a new tracker for the given item hash.
    pub fn new(item_hash: Hash, config: ItemFetcherConfig) -> Self {
        Self {
            item_hash,
            config,
            peers_asked: HashMap::new(),
            last_asked_peer: None,
            waiting_envelopes: Vec::new(),
            fetch_start: Instant::now(),
            last_ask_time: None,
            num_list_rebuild: 0,
            last_seen_slot_index: 0,
        }
    }

    /// Returns true if no envelopes are waiting.
    pub fn is_empty(&self) -> bool {
        self.waiting_envelopes.is_empty()
    }

    /// Returns the number of waiting envelopes.
    pub fn len(&self) -> usize {
        self.waiting_envelopes.len()
    }

    /// Get the hash of the item being tracked.
    pub fn item_hash(&self) -> &Hash {
        &self.item_hash
    }

    /// Get waiting envelopes.
    pub fn waiting_envelopes(&self) -> &[(Hash, ScpEnvelope)] {
        &self.waiting_envelopes
    }

    /// Pop an envelope from the list.
    pub fn pop(&mut self) -> Option<ScpEnvelope> {
        self.waiting_envelopes.pop().map(|(_, env)| env)
    }

    /// Get duration since fetch started.
    pub fn get_duration(&self) -> Duration {
        self.fetch_start.elapsed()
    }

    /// Get the last seen slot index.
    pub fn last_seen_slot_index(&self) -> u64 {
        self.last_seen_slot_index
    }

    /// Reset the last seen slot index.
    pub fn reset_last_seen_slot_index(&mut self) {
        self.last_seen_slot_index = 0;
    }

    /// Clear envelopes below a certain slot index.
    ///
    /// Returns true if at least one envelope remains.
    pub fn clear_envelopes_below(&mut self, slot_index: u64, slot_to_keep: u64) -> bool {
        self.waiting_envelopes.retain(|(_, env)| {
            let idx = env.statement.slot_index;
            idx >= slot_index || idx == slot_to_keep
        });

        if self.waiting_envelopes.is_empty() {
            self.cancel();
            false
        } else {
            true
        }
    }

    /// Add an envelope to the waiting list.
    // SECURITY: listener count bounded by authenticated peer count
    pub fn listen(&mut self, env: &ScpEnvelope) {
        self.last_seen_slot_index = self.last_seen_slot_index.max(env.statement.slot_index);

        // Don't track the same envelope twice
        let env_hash = compute_envelope_hash(env);
        if self.waiting_envelopes.iter().any(|(h, _)| h == &env_hash) {
            return;
        }

        self.waiting_envelopes.push((env_hash, env.clone()));
    }

    /// Stop tracking an envelope.
    pub fn discard(&mut self, env: &ScpEnvelope) {
        let env_hash = compute_envelope_hash(env);
        self.waiting_envelopes.retain(|(h, _)| h != &env_hash);
    }

    /// Cancel fetching.
    pub fn cancel(&mut self) {
        self.last_ask_time = None;
        self.last_seen_slot_index = 0;
    }

    /// Clear all state.
    /// Called after catchup to release memory.
    pub fn clear(&mut self) {
        self.waiting_envelopes.clear();
        self.last_ask_time = None;
        self.last_seen_slot_index = 0;
    }

    /// Handle a DONT_HAVE response from a peer.
    pub fn doesnt_have(&mut self, peer: &PeerId) -> bool {
        if self.last_asked_peer.as_ref() == Some(peer) {
            trace!(
                "Peer {} does not have {}",
                peer,
                hex::encode(self.item_hash.0)
            );
            self.last_asked_peer = None;
            true
        } else {
            false
        }
    }

    /// Check if we can ask a peer.
    fn can_ask_peer(&self, peer: &PeerId, peer_has: bool) -> bool {
        match self.peers_asked.get(peer) {
            None => true,
            Some(had_before) => peer_has && !had_before,
        }
    }

    /// Select the next peer to ask.
    ///
    /// Returns the peer to ask, or None if we need to wait/rebuild.
    /// Also returns how long to wait before asking again.
    // SECURITY: retry state bounded by peer count and flow control
    pub fn try_next_peer(&mut self, available_peers: &[PeerId]) -> NextPeerResult {
        trace!(
            "tryNextPeer {} last: {:?}",
            hex::encode(self.item_hash.0),
            self.last_asked_peer
        );

        if self.last_asked_peer.is_some() {
            self.last_asked_peer = None;
        }

        // Find peers we haven't asked yet
        let candidates: Vec<&PeerId> = available_peers
            .iter()
            .filter(|p| self.can_ask_peer(p, false))
            .collect();

        if let Some(&peer) = candidates.first() {
            // For simplicity, just pick the first candidate
            // In stellar-core, there's latency-based selection, but we simplify here
            self.last_asked_peer = Some(peer.clone());
            self.peers_asked.insert(peer.clone(), false);
            self.last_ask_time = Some(Instant::now());

            trace!("Asking peer {} for {}", peer, hex::encode(self.item_hash.0));

            NextPeerResult::AskPeer {
                peer: peer.clone(),
                timeout: self.config.fetch_reply_timeout,
            }
        } else {
            // We've asked all peers, rebuild the list
            self.num_list_rebuild += 1;
            self.peers_asked.clear();

            trace!(
                "tryNextPeer {} restarting fetch #{}",
                hex::encode(self.item_hash.0),
                self.num_list_rebuild
            );

            let wait_time = self.config.fetch_reply_timeout
                * self
                    .num_list_rebuild
                    .min(self.config.max_rebuild_fetch_list);

            NextPeerResult::Wait {
                duration: wait_time,
            }
        }
    }

    /// Check if the fetch has timed out waiting for the current peer.
    pub fn is_timed_out(&self) -> bool {
        if let Some(ask_time) = self.last_ask_time {
            ask_time.elapsed() >= self.config.fetch_reply_timeout
        } else {
            false
        }
    }
}

/// Result of trying to get the next peer.
#[derive(Debug)]
pub enum NextPeerResult {
    /// Ask this peer for the item.
    AskPeer { peer: PeerId, timeout: Duration },
    /// Wait before trying again.
    Wait { duration: Duration },
}

/// Item fetcher manages fetching TxSets and QuorumSets from peers.
///
/// Thread-safe manager that maintains trackers for items being fetched.
pub struct ItemFetcher {
    /// Configuration.
    config: ItemFetcherConfig,
    /// Item type this fetcher handles.
    item_type: ItemType,
    /// Trackers by item hash.
    trackers: Mutex<HashMap<Hash, Tracker>>,
    /// Callback to request items from peers.
    ask_peer: Option<AskPeerFn>,
    /// Available peers (updated externally).
    available_peers: Mutex<Vec<PeerId>>,
    /// Optional overlay metrics (None when used outside overlay, e.g. herder).
    metrics: Option<Arc<OverlayMetrics>>,
}

impl ItemFetcher {
    /// Create a new item fetcher.
    pub fn new(
        item_type: ItemType,
        config: ItemFetcherConfig,
        metrics: Option<Arc<OverlayMetrics>>,
    ) -> Self {
        Self {
            config,
            item_type,
            trackers: Mutex::new(HashMap::new()),
            ask_peer: None,
            available_peers: Mutex::new(Vec::new()),
            metrics,
        }
    }

    /// Create a new item fetcher with default config.
    pub fn with_defaults(item_type: ItemType, metrics: Option<Arc<OverlayMetrics>>) -> Self {
        Self::new(item_type, ItemFetcherConfig::default(), metrics)
    }

    /// Set the callback for asking peers.
    pub fn set_ask_peer(&mut self, f: AskPeerFn) {
        self.ask_peer = Some(f);
    }

    /// Update the list of available peers.
    pub fn set_available_peers(&self, peers: Vec<PeerId>) {
        *self.available_peers.lock().unwrap() = peers;
    }

    /// Get the current list of available peers.
    pub fn get_available_peers(&self) -> Vec<PeerId> {
        self.available_peers.lock().unwrap().clone()
    }

    /// Start fetching an item needed by an envelope.
    ///
    /// Multiple envelopes may need the same item.
    /// Immediately tries to fetch from a peer if callback is set.
    pub fn fetch(&self, item_hash: Hash, envelope: &ScpEnvelope) {
        let available_peers = self.available_peers.lock().unwrap().clone();
        let mut trackers = self.trackers.lock().unwrap();

        trace!("fetch {:?} {}", self.item_type, hex::encode(item_hash.0));

        if let Some(tracker) = trackers.get_mut(&item_hash) {
            // Already tracking, just add the envelope
            tracker.listen(envelope);
        } else {
            // Create new tracker
            let mut tracker = Tracker::new(item_hash.clone(), self.config.clone());
            tracker.listen(envelope);

            // Immediately try to fetch from a peer (like stellar-core)
            if let Some(ref ask_peer) = self.ask_peer {
                match tracker.try_next_peer(&available_peers) {
                    NextPeerResult::AskPeer { ref peer, .. } => {
                        if let Some(ref m) = self.metrics {
                            m.item_fetcher_next_peer.inc();
                        }
                        trace!(
                            "Immediately asking peer {} for {:?} {}",
                            peer,
                            self.item_type,
                            hex::encode(item_hash.0)
                        );
                        ask_peer(peer, &item_hash, self.item_type);
                    }
                    NextPeerResult::Wait { .. } => {
                        // No peers available yet, will retry later
                    }
                }
            }

            trackers.insert(item_hash, tracker);
        }
    }

    /// Stop fetching an item for a specific envelope.
    ///
    /// If other envelopes still need this item, fetching continues.
    // SECURITY: fetch tracker state bounded by authenticated peer count and flow control windows
    pub fn stop_fetch(&self, item_hash: &Hash, envelope: &ScpEnvelope) {
        let mut trackers = self.trackers.lock().unwrap();

        if let Some(tracker) = trackers.get_mut(item_hash) {
            trace!(
                "stopFetch {:?} {} : {}",
                self.item_type,
                hex::encode(item_hash.0),
                tracker.len()
            );

            tracker.discard(envelope);

            if tracker.is_empty() {
                tracker.cancel();
            }
        } else {
            trace!(
                "stopFetch untracked {:?} {}",
                self.item_type,
                hex::encode(item_hash.0)
            );
        }
    }

    /// Get the last seen slot index for an item.
    pub fn get_last_seen_slot_index(&self, item_hash: &Hash) -> u64 {
        let trackers = self.trackers.lock().unwrap();
        trackers
            .get(item_hash)
            .map(|t| t.last_seen_slot_index())
            .unwrap_or(0)
    }

    /// Get envelopes waiting for an item.
    pub fn fetching_for(&self, item_hash: &Hash) -> Vec<ScpEnvelope> {
        let trackers = self.trackers.lock().unwrap();
        trackers
            .get(item_hash)
            .map(|t| {
                t.waiting_envelopes()
                    .iter()
                    .map(|(_, env)| env.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Stop fetching items for slots below a threshold.
    pub fn stop_fetching_below(&self, slot_index: u64, slot_to_keep: u64) {
        let mut trackers = self.trackers.lock().unwrap();

        trackers.retain(|_, tracker| tracker.clear_envelopes_below(slot_index, slot_to_keep));
    }

    /// Handle a DONT_HAVE response from a peer.
    pub fn doesnt_have(&self, item_hash: &Hash, peer: &PeerId) {
        let mut trackers = self.trackers.lock().unwrap();

        if let Some(tracker) = trackers.get_mut(item_hash) {
            tracker.doesnt_have(peer);
        }
    }

    /// Called when an item is received.
    ///
    /// Atomically removes the tracker from the map and returns the waiting
    /// envelopes. Returns `None` if no tracker exists (unsolicited item).
    ///
    /// By removing the tracker, `is_tracking()` becomes authoritative: it
    /// returns `false` immediately after receive, preventing duplicate
    /// processing by any caller.
    // SECURITY: fetch tracker state bounded by authenticated peer count and flow control windows
    pub fn recv(&self, item_hash: &Hash) -> Option<Vec<ScpEnvelope>> {
        let mut trackers = self.trackers.lock().unwrap();

        let tracker = trackers.remove(item_hash)?;

        trace!(
            "Recv {:?} {} : {}",
            self.item_type,
            hex::encode(item_hash.0),
            tracker.len()
        );

        let duration = tracker.get_duration();
        debug!(
            "Fetched {:?} {} in {:?}",
            self.item_type,
            hex::encode(item_hash.0),
            duration
        );

        let envelopes: Vec<ScpEnvelope> = tracker
            .waiting_envelopes
            .into_iter()
            .map(|(_, env)| env)
            .collect();

        Some(envelopes)
    }

    /// Get items that need to be requested from peers.
    ///
    /// Returns (item_hash, peer_to_ask) pairs.
    // SECURITY: retry state bounded by peer count and flow control
    pub fn get_pending_requests(&self, available_peers: &[PeerId]) -> Vec<PendingRequest> {
        let mut trackers = self.trackers.lock().unwrap();
        let mut requests = Vec::new();

        for (hash, tracker) in trackers.iter_mut() {
            if tracker.is_empty() {
                continue;
            }

            // Check if we need to ask a new peer (timed out or no current peer)
            if tracker.last_asked_peer.is_none() || tracker.is_timed_out() {
                match tracker.try_next_peer(available_peers) {
                    NextPeerResult::AskPeer { peer, timeout } => {
                        if let Some(ref m) = self.metrics {
                            m.item_fetcher_next_peer.inc();
                        }
                        requests.push(PendingRequest {
                            item_hash: hash.clone(),
                            peer,
                            timeout,
                        });
                    }
                    NextPeerResult::Wait { .. } => {
                        // Will retry later
                    }
                }
            }
        }

        requests
    }

    /// Process pending requests and invoke callbacks for items that need fetching.
    ///
    /// This should be called periodically (e.g., every second) to handle timeouts
    /// and retry fetching from different peers. Returns the number of requests sent.
    pub fn process_pending(&self) -> usize {
        let available_peers = self.available_peers.lock().unwrap().clone();
        let requests = self.get_pending_requests(&available_peers);

        if requests.is_empty() {
            return 0;
        }

        let mut sent = 0;
        if let Some(ref ask_peer) = self.ask_peer {
            for request in &requests {
                trace!(
                    "Processing pending {:?} {} -> peer {}",
                    self.item_type,
                    hex::encode(request.item_hash.0),
                    request.peer
                );
                ask_peer(&request.peer, &request.item_hash, self.item_type);
                sent += 1;
            }
        }

        debug!("Processed {} pending {:?} requests", sent, self.item_type);
        sent
    }

    /// Check if an item is being tracked.
    pub fn is_tracking(&self, item_hash: &Hash) -> bool {
        let trackers = self.trackers.lock().unwrap();
        trackers.contains_key(item_hash)
    }

    /// Get the number of items being tracked.
    pub fn num_trackers(&self) -> usize {
        let trackers = self.trackers.lock().unwrap();
        trackers.len()
    }

    /// Get statistics.
    pub fn get_stats(&self) -> ItemFetcherStats {
        let trackers = self.trackers.lock().unwrap();

        let mut total_waiting = 0;
        let mut oldest_duration = Duration::ZERO;

        for tracker in trackers.values() {
            total_waiting += tracker.len();
            let dur = tracker.get_duration();
            if dur > oldest_duration {
                oldest_duration = dur;
            }
        }

        ItemFetcherStats {
            item_type: self.item_type,
            num_trackers: trackers.len(),
            total_waiting_envelopes: total_waiting,
            oldest_fetch_duration: oldest_duration,
        }
    }

    /// Clear all trackers and pending state.
    /// Called after catchup to release memory.
    pub fn clear(&self) {
        let mut trackers = self.trackers.lock().unwrap();
        let count = trackers.len();
        trackers.clear();
        if count > 0 {
            debug!(item_type = ?self.item_type, count, "Cleared item fetcher trackers");
        }
    }
}

/// A pending fetch request.
#[derive(Debug, Clone)]
pub struct PendingRequest {
    /// Hash of the item to fetch.
    pub item_hash: Hash,
    /// Peer to ask.
    pub peer: PeerId,
    /// Timeout for the request.
    pub timeout: Duration,
}

/// Statistics about item fetching.
#[derive(Debug, Clone)]
pub struct ItemFetcherStats {
    /// Type of items being fetched.
    pub item_type: ItemType,
    /// Number of items being tracked.
    pub num_trackers: usize,
    /// Total envelopes waiting across all trackers.
    pub total_waiting_envelopes: usize,
    /// Duration of the oldest fetch.
    pub oldest_fetch_duration: Duration,
}

/// Compute hash of an SCP envelope for tracking.
///
/// Uses the same approach as stellar-core: BLAKE2b-256 of the StellarMessage wrapping the envelope.
fn compute_envelope_hash(env: &ScpEnvelope) -> Hash {
    // Create a StellarMessage::ScpMessage wrapping the envelope
    let msg = stellar_xdr::curr::StellarMessage::ScpMessage(env.clone());

    // Serialize to XDR
    let xdr_bytes = henyey_common::xdr_to_bytes(&msg);

    // Compute BLAKE2b-256 hash (matching stellar-core's xdrBlake2)
    let hash256 = henyey_crypto::blake2(&xdr_bytes);
    Hash(*hash256.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::{
        NodeId, PublicKey, ScpNomination, ScpStatement, ScpStatementPledges, Signature, Uint256,
    };

    fn make_test_envelope(slot_index: u64) -> ScpEnvelope {
        ScpEnvelope {
            statement: ScpStatement {
                node_id: NodeId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
                slot_index,
                pledges: ScpStatementPledges::Nominate(ScpNomination {
                    quorum_set_hash: Hash([0u8; 32]),
                    votes: vec![].try_into().unwrap(),
                    accepted: vec![].try_into().unwrap(),
                }),
            },
            signature: Signature::default(),
        }
    }

    fn make_peer_id(id: u8) -> PeerId {
        PeerId::from_bytes([id; 32])
    }

    #[test]
    fn test_tracker_creation() {
        let hash = Hash([1u8; 32]);
        let tracker = Tracker::new(hash.clone(), ItemFetcherConfig::default());

        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
        assert_eq!(tracker.item_hash(), &hash);
    }

    #[test]
    fn test_tracker_listen() {
        let hash = Hash([1u8; 32]);
        let mut tracker = Tracker::new(hash, ItemFetcherConfig::default());
        let env = make_test_envelope(100);

        tracker.listen(&env);

        assert!(!tracker.is_empty());
        assert_eq!(tracker.len(), 1);
        assert_eq!(tracker.last_seen_slot_index(), 100);

        // Listening to same envelope again should not add it
        tracker.listen(&env);
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn test_tracker_discard() {
        let hash = Hash([1u8; 32]);
        let mut tracker = Tracker::new(hash, ItemFetcherConfig::default());
        let env = make_test_envelope(100);

        tracker.listen(&env);
        assert_eq!(tracker.len(), 1);

        tracker.discard(&env);
        assert!(tracker.is_empty());
    }

    #[test]
    fn test_tracker_clear_below() {
        let hash = Hash([1u8; 32]);
        let mut tracker = Tracker::new(hash, ItemFetcherConfig::default());

        tracker.listen(&make_test_envelope(100));
        tracker.listen(&make_test_envelope(200));
        tracker.listen(&make_test_envelope(300));

        // Clear below 201, keep 100
        // This removes slot 200 (< 201 and != 100), keeps 100 (== keep_slot) and 300 (>= 201)
        let has_more = tracker.clear_envelopes_below(201, 100);

        assert!(has_more);
        assert_eq!(tracker.len(), 2); // 100 (kept) and 300
    }

    #[test]
    fn test_tracker_try_next_peer() {
        let hash = Hash([1u8; 32]);
        let mut tracker = Tracker::new(hash, ItemFetcherConfig::default());
        let peers = vec![make_peer_id(1), make_peer_id(2), make_peer_id(3)];

        // First call should give us a peer
        let result = tracker.try_next_peer(&peers);
        match result {
            NextPeerResult::AskPeer { peer, .. } => {
                assert_eq!(peer, peers[0]);
            }
            NextPeerResult::Wait { .. } => panic!("Expected AskPeer"),
        }

        // Simulate DONT_HAVE and try again
        tracker.doesnt_have(&peers[0]);
        let result = tracker.try_next_peer(&peers);
        match result {
            NextPeerResult::AskPeer { peer, .. } => {
                assert_eq!(peer, peers[1]);
            }
            NextPeerResult::Wait { .. } => panic!("Expected AskPeer"),
        }
    }

    #[test]
    fn test_tracker_rebuild_list() {
        let hash = Hash([1u8; 32]);
        let mut tracker = Tracker::new(hash, ItemFetcherConfig::default());
        let peers = vec![make_peer_id(1)];

        // Ask peer 1
        let result = tracker.try_next_peer(&peers);
        assert!(matches!(result, NextPeerResult::AskPeer { .. }));

        // All peers asked, should rebuild
        tracker.doesnt_have(&peers[0]);
        let result = tracker.try_next_peer(&peers);
        assert!(matches!(result, NextPeerResult::Wait { .. }));
    }

    #[test]
    fn test_item_fetcher_fetch() {
        let fetcher = ItemFetcher::with_defaults(ItemType::TxSet, None);
        let hash = Hash([1u8; 32]);
        let env = make_test_envelope(100);

        fetcher.fetch(hash.clone(), &env);

        assert!(fetcher.is_tracking(&hash));
        assert_eq!(fetcher.num_trackers(), 1);

        let envelopes = fetcher.fetching_for(&hash);
        assert_eq!(envelopes.len(), 1);
    }

    #[test]
    fn test_item_fetcher_recv() {
        let fetcher = ItemFetcher::with_defaults(ItemType::QuorumSet, None);
        let hash = Hash([1u8; 32]);
        let env = make_test_envelope(100);

        fetcher.fetch(hash.clone(), &env);

        let envelopes = fetcher.recv(&hash).expect("should be tracking");

        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].statement.slot_index, 100);
    }

    #[test]
    fn test_item_fetcher_stop_fetch() {
        let fetcher = ItemFetcher::with_defaults(ItemType::TxSet, None);
        let hash = Hash([1u8; 32]);
        let env = make_test_envelope(100);

        fetcher.fetch(hash.clone(), &env);
        assert!(fetcher.is_tracking(&hash));

        fetcher.stop_fetch(&hash, &env);
        // Tracker may still exist but be empty
        assert_eq!(fetcher.fetching_for(&hash).len(), 0);
    }

    #[test]
    fn test_item_fetcher_doesnt_have() {
        let fetcher = ItemFetcher::with_defaults(ItemType::TxSet, None);
        let hash = Hash([1u8; 32]);
        let env = make_test_envelope(100);
        let peer = make_peer_id(1);

        fetcher.fetch(hash.clone(), &env);

        // This shouldn't panic
        fetcher.doesnt_have(&hash, &peer);
    }

    #[test]
    fn test_item_fetcher_pending_requests() {
        let fetcher = ItemFetcher::with_defaults(ItemType::TxSet, None);
        let hash = Hash([1u8; 32]);
        let env = make_test_envelope(100);
        let peers = vec![make_peer_id(1), make_peer_id(2)];

        fetcher.fetch(hash.clone(), &env);

        let requests = fetcher.get_pending_requests(&peers);

        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].item_hash, hash);
    }

    #[test]
    fn test_item_fetcher_stats() {
        let fetcher = ItemFetcher::with_defaults(ItemType::QuorumSet, None);
        let hash1 = Hash([1u8; 32]);
        let hash2 = Hash([2u8; 32]);
        let env1 = make_test_envelope(100);
        let env2 = make_test_envelope(200);

        fetcher.fetch(hash1, &env1);
        fetcher.fetch(hash2, &env2);

        let stats = fetcher.get_stats();

        assert_eq!(stats.item_type, ItemType::QuorumSet);
        assert_eq!(stats.num_trackers, 2);
        assert_eq!(stats.total_waiting_envelopes, 2);
    }
}
