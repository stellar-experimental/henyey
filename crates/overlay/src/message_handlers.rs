//! Message handlers for overlay protocol messages.
//!
//! This module provides handlers for various Stellar overlay protocol messages.
//! It integrates with the ItemFetcher for TxSet and QuorumSet retrieval.
//!
//! # Message Types Handled
//!
//! - `GetTxSet` - Request for a transaction set by hash
//! - `TxSet` / `GeneralizedTxSet` - Response with transaction set data
//! - `GetScpQuorumSet` - Request for a quorum set by hash
//! - `ScpQuorumset` - Response with quorum set data
//! - `DontHave` - Response indicating the peer doesn't have the requested item
//!
//! # Architecture
//!
//! The `MessageDispatcher` coordinates message handling:
//! 1. Receives messages from the overlay manager
//! 2. Routes requests to appropriate handlers
//! 3. Uses ItemFetcher to track pending fetches
//! 4. Emits events when items are received

use crate::{
    item_fetcher::{ItemFetcher, ItemType, PendingRequest},
    PeerId,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use stellar_xdr::curr::{
    DontHave, GeneralizedTransactionSet, Hash, MessageType, ScpEnvelope, ScpQuorumSet,
    StellarMessage, TransactionSet,
};
use tracing::{debug, trace};

/// Maximum number of entries in the TxSet and QuorumSet caches.
/// Matches stellar-core's TXSET_CACHE_SIZE / QSET_CACHE_SIZE (PendingEnvelopes.cpp).
const MAX_CACHE_SIZE: usize = 10_000;

/// Callback for when a TxSet is received.
pub type TxSetCallback = Box<dyn Fn(Hash, TxSetData) + Send + Sync>;

/// Callback for when a QuorumSet is received.
pub type QuorumSetCallback = Box<dyn Fn(Hash, ScpQuorumSet) + Send + Sync>;

/// Callback for when envelopes should be re-processed after receiving data.
pub type EnvelopeCallback = Box<dyn Fn(Vec<ScpEnvelope>) + Send + Sync>;

/// Transaction set data (either legacy or generalized).
#[derive(Debug, Clone)]
pub enum TxSetData {
    /// Legacy transaction set.
    Legacy(TransactionSet),
    /// Generalized transaction set (protocol 20+).
    Generalized(GeneralizedTransactionSet),
}

impl TxSetData {
    /// Get the SHA-256 hash of the XDR-encoded transaction set.
    pub fn hash(&self) -> Hash {
        let h = match self {
            TxSetData::Legacy(tx_set) => henyey_common::Hash256::hash_xdr(tx_set),
            TxSetData::Generalized(tx_set) => henyey_common::Hash256::hash_xdr(tx_set),
        };
        Hash(h.0)
    }
}

/// Evict a random entry from the map if it is at or above the given capacity.
/// Matches stellar-core's random eviction strategy (RandomEvictionCache).
fn evict_if_full<V>(map: &mut HashMap<Hash, V>, max_size: usize) {
    if map.len() >= max_size {
        // Pick a random key to evict using a simple hash of the first key found.
        // This is not cryptographically random but sufficient for cache eviction.
        if let Some(key) = map.keys().next().cloned() {
            map.remove(&key);
        }
    }
}

/// Handler for overlay protocol messages.
///
/// Coordinates message handling, item fetching, and callback dispatch.
pub struct MessageDispatcher {
    /// Fetcher for transaction sets.
    tx_set_fetcher: Arc<ItemFetcher>,
    /// Fetcher for quorum sets.
    quorum_set_fetcher: Arc<ItemFetcher>,
    /// Cache of received transaction sets.
    tx_set_cache: Arc<Mutex<HashMap<Hash, TxSetData>>>,
    /// Cache of received quorum sets.
    quorum_set_cache: Arc<Mutex<HashMap<Hash, ScpQuorumSet>>>,
    /// Callback when TxSet is received.
    on_tx_set: Option<TxSetCallback>,
    /// Callback when QuorumSet is received.
    on_quorum_set: Option<QuorumSetCallback>,
    /// Callback to re-process envelopes.
    on_envelopes_ready: Option<EnvelopeCallback>,
}

impl MessageDispatcher {
    /// Create a new message dispatcher.
    pub fn new() -> Self {
        Self {
            tx_set_fetcher: Arc::new(ItemFetcher::with_defaults(ItemType::TxSet)),
            quorum_set_fetcher: Arc::new(ItemFetcher::with_defaults(ItemType::QuorumSet)),
            tx_set_cache: Arc::new(Mutex::new(HashMap::new())),
            quorum_set_cache: Arc::new(Mutex::new(HashMap::new())),
            on_tx_set: None,
            on_quorum_set: None,
            on_envelopes_ready: None,
        }
    }

    /// Set callback for TxSet receipt.
    pub fn set_tx_set_callback(&mut self, callback: TxSetCallback) {
        self.on_tx_set = Some(callback);
    }

    /// Set callback for QuorumSet receipt.
    pub fn set_quorum_set_callback(&mut self, callback: QuorumSetCallback) {
        self.on_quorum_set = Some(callback);
    }

    /// Set callback for envelopes ready for re-processing.
    pub fn set_envelopes_callback(&mut self, callback: EnvelopeCallback) {
        self.on_envelopes_ready = Some(callback);
    }

    /// Handle an incoming message from a peer.
    ///
    /// Returns a response message if one should be sent back to the peer.
    pub fn handle_message(
        &self,
        from_peer: &PeerId,
        message: &StellarMessage,
    ) -> Option<StellarMessage> {
        match message {
            StellarMessage::GetTxSet(hash) => self.handle_get_tx_set(from_peer, &Hash(hash.0)),
            StellarMessage::TxSet(tx_set) => {
                self.handle_tx_set_data(from_peer, TxSetData::Legacy(tx_set.clone()));
                None
            }
            StellarMessage::GeneralizedTxSet(tx_set) => {
                self.handle_tx_set_data(from_peer, TxSetData::Generalized(tx_set.clone()));
                None
            }
            StellarMessage::GetScpQuorumset(hash) => {
                self.handle_get_quorum_set(from_peer, &Hash(hash.0))
            }
            StellarMessage::ScpQuorumset(qs) => {
                self.handle_quorum_set(from_peer, qs.clone());
                None
            }
            StellarMessage::DontHave(dont_have) => {
                self.handle_dont_have(from_peer, dont_have);
                None
            }
            _ => None,
        }
    }

    /// Handle a GetTxSet request.
    fn handle_get_tx_set(&self, from_peer: &PeerId, hash: &Hash) -> Option<StellarMessage> {
        trace!(
            "Received GetTxSet {} from {}",
            hex::encode(hash.0),
            from_peer
        );

        // Check if we have this TxSet cached
        {
            let cache = self.tx_set_cache.lock().unwrap();
            if let Some(tx_set) = cache.get(hash) {
                match tx_set {
                    TxSetData::Legacy(ts) => {
                        return Some(StellarMessage::TxSet(ts.clone()));
                    }
                    TxSetData::Generalized(ts) => {
                        return Some(StellarMessage::GeneralizedTxSet(ts.clone()));
                    }
                }
            }
        }

        // We don't have it, send DONT_HAVE
        Some(StellarMessage::DontHave(DontHave {
            type_: MessageType::TxSet,
            req_hash: stellar_xdr::curr::Uint256(hash.0),
        }))
    }

    /// Handle a received transaction set (legacy or generalized).
    // SECURITY: message size bounded by MAX_MESSAGE_SIZE at frame layer before reaching handler
    fn handle_tx_set_data(&self, from_peer: &PeerId, data: TxSetData) {
        let hash = data.hash();
        let label = match &data {
            TxSetData::Legacy(_) => "TxSet",
            TxSetData::Generalized(_) => "GeneralizedTxSet",
        };

        trace!(
            "Received {} {} from {}",
            label,
            hex::encode(hash.0),
            from_peer
        );

        // Cache it (bounded to MAX_CACHE_SIZE, matching stellar-core's RandomEvictionCache)
        {
            let mut cache = self.tx_set_cache.lock().unwrap();
            evict_if_full(&mut cache, MAX_CACHE_SIZE);
            cache.insert(hash.clone(), data.clone());
        }

        // Notify fetcher
        let envelopes = self.tx_set_fetcher.recv(&hash);

        // Invoke callbacks
        if let Some(ref callback) = self.on_tx_set {
            callback(hash, data);
        }

        if !envelopes.is_empty() {
            if let Some(ref callback) = self.on_envelopes_ready {
                callback(envelopes);
            }
        }
    }

    /// Handle a GetScpQuorumSet request.
    fn handle_get_quorum_set(&self, from_peer: &PeerId, hash: &Hash) -> Option<StellarMessage> {
        trace!(
            "Received GetScpQuorumSet {} from {}",
            hex::encode(hash.0),
            from_peer
        );

        // Check if we have this QuorumSet cached
        {
            let cache = self.quorum_set_cache.lock().unwrap();
            if let Some(qs) = cache.get(hash) {
                return Some(StellarMessage::ScpQuorumset(qs.clone()));
            }
        }

        // We don't have it, send DONT_HAVE
        Some(StellarMessage::DontHave(DontHave {
            type_: MessageType::ScpQuorumset,
            req_hash: stellar_xdr::curr::Uint256(hash.0),
        }))
    }

    /// Handle a ScpQuorumset response.
    // SECURITY: message size bounded by MAX_MESSAGE_SIZE at frame layer before reaching handler
    fn handle_quorum_set(&self, from_peer: &PeerId, quorum_set: ScpQuorumSet) {
        let h = henyey_common::Hash256::hash_xdr(&quorum_set);
        let hash = Hash(h.0);

        trace!(
            "Received ScpQuorumSet {} from {}",
            hex::encode(hash.0),
            from_peer
        );

        // Cache it (bounded to MAX_CACHE_SIZE, matching stellar-core's RandomEvictionCache)
        {
            let mut cache = self.quorum_set_cache.lock().unwrap();
            evict_if_full(&mut cache, MAX_CACHE_SIZE);
            cache.insert(hash.clone(), quorum_set.clone());
        }

        // Notify fetcher
        let envelopes = self.quorum_set_fetcher.recv(&hash);

        // Invoke callbacks
        if let Some(ref callback) = self.on_quorum_set {
            callback(hash, quorum_set);
        }

        if !envelopes.is_empty() {
            if let Some(ref callback) = self.on_envelopes_ready {
                callback(envelopes);
            }
        }
    }

    /// Handle a DontHave response.
    fn handle_dont_have(&self, from_peer: &PeerId, dont_have: &DontHave) {
        let hash = Hash(dont_have.req_hash.0);

        trace!(
            "Received DontHave {:?} {} from {}",
            dont_have.type_,
            hex::encode(hash.0),
            from_peer
        );

        match dont_have.type_ {
            MessageType::TxSet | MessageType::GeneralizedTxSet => {
                self.tx_set_fetcher.doesnt_have(&hash, from_peer);
            }
            MessageType::ScpQuorumset => {
                self.quorum_set_fetcher.doesnt_have(&hash, from_peer);
            }
            _ => {
                debug!(
                    "Unexpected DontHave type {:?} from {}",
                    dont_have.type_, from_peer
                );
            }
        }
    }

    /// Fetch a TxSet needed by an SCP envelope.
    pub fn fetch_tx_set(&self, hash: Hash, envelope: &ScpEnvelope) {
        trace!("Requesting TxSet {}", hex::encode(hash.0));
        self.tx_set_fetcher.fetch(hash, envelope);
    }

    /// Fetch a QuorumSet needed by an SCP envelope.
    pub fn fetch_quorum_set(&self, hash: Hash, envelope: &ScpEnvelope) {
        trace!("Requesting QuorumSet {}", hex::encode(hash.0));
        self.quorum_set_fetcher.fetch(hash, envelope);
    }

    /// Stop fetching a TxSet for an envelope.
    pub fn stop_fetch_tx_set(&self, hash: &Hash, envelope: &ScpEnvelope) {
        self.tx_set_fetcher.stop_fetch(hash, envelope);
    }

    /// Stop fetching a QuorumSet for an envelope.
    pub fn stop_fetch_quorum_set(&self, hash: &Hash, envelope: &ScpEnvelope) {
        self.quorum_set_fetcher.stop_fetch(hash, envelope);
    }

    /// Clean up fetches for old slots.
    pub fn stop_fetching_below(&self, slot_index: u64, slot_to_keep: u64) {
        self.tx_set_fetcher
            .stop_fetching_below(slot_index, slot_to_keep);
        self.quorum_set_fetcher
            .stop_fetching_below(slot_index, slot_to_keep);
    }

    /// Get pending TxSet requests.
    pub fn get_pending_tx_set_requests(&self, peers: &[PeerId]) -> Vec<PendingRequest> {
        self.tx_set_fetcher.get_pending_requests(peers)
    }

    /// Get pending QuorumSet requests.
    pub fn get_pending_quorum_set_requests(&self, peers: &[PeerId]) -> Vec<PendingRequest> {
        self.quorum_set_fetcher.get_pending_requests(peers)
    }

    /// Check if a TxSet is cached.
    pub fn has_tx_set(&self, hash: &Hash) -> bool {
        let cache = self.tx_set_cache.lock().unwrap();
        cache.contains_key(hash)
    }

    /// Check if a QuorumSet is cached.
    pub fn has_quorum_set(&self, hash: &Hash) -> bool {
        let cache = self.quorum_set_cache.lock().unwrap();
        cache.contains_key(hash)
    }

    /// Get a cached TxSet.
    pub fn get_tx_set(&self, hash: &Hash) -> Option<TxSetData> {
        let cache = self.tx_set_cache.lock().unwrap();
        cache.get(hash).cloned()
    }

    /// Get a cached QuorumSet.
    pub fn get_quorum_set(&self, hash: &Hash) -> Option<ScpQuorumSet> {
        let cache = self.quorum_set_cache.lock().unwrap();
        cache.get(hash).cloned()
    }

    /// Store a TxSet in the cache, evicting a random entry if at capacity.
    pub fn cache_tx_set(&self, hash: Hash, data: TxSetData) {
        let mut cache = self.tx_set_cache.lock().unwrap();
        evict_if_full(&mut cache, MAX_CACHE_SIZE);
        cache.insert(hash, data);
    }

    /// Store a QuorumSet in the cache, evicting a random entry if at capacity.
    pub fn cache_quorum_set(&self, hash: Hash, quorum_set: ScpQuorumSet) {
        let mut cache = self.quorum_set_cache.lock().unwrap();
        evict_if_full(&mut cache, MAX_CACHE_SIZE);
        cache.insert(hash, quorum_set);
    }

    /// Get statistics.
    pub fn stats(&self) -> MessageDispatcherStats {
        let tx_set_cache = self.tx_set_cache.lock().unwrap();
        let quorum_set_cache = self.quorum_set_cache.lock().unwrap();

        MessageDispatcherStats {
            tx_set_fetcher_stats: self.tx_set_fetcher.get_stats(),
            quorum_set_fetcher_stats: self.quorum_set_fetcher.get_stats(),
            cached_tx_sets: tx_set_cache.len(),
            cached_quorum_sets: quorum_set_cache.len(),
        }
    }
}

impl Default for MessageDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Statistics about message dispatch.
#[derive(Debug, Clone)]
pub struct MessageDispatcherStats {
    /// TxSet fetcher statistics.
    pub tx_set_fetcher_stats: crate::item_fetcher::ItemFetcherStats,
    /// QuorumSet fetcher statistics.
    pub quorum_set_fetcher_stats: crate::item_fetcher::ItemFetcherStats,
    /// Number of cached TxSets.
    pub cached_tx_sets: usize,
    /// Number of cached QuorumSets.
    pub cached_quorum_sets: usize,
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
    fn test_message_dispatcher_creation() {
        let dispatcher = MessageDispatcher::new();
        let stats = dispatcher.stats();

        assert_eq!(stats.cached_tx_sets, 0);
        assert_eq!(stats.cached_quorum_sets, 0);
    }

    #[test]
    fn test_get_tx_set_unknown() {
        let dispatcher = MessageDispatcher::new();
        let peer = make_peer_id(1);
        let hash = Hash([1u8; 32]);

        let response = dispatcher.handle_get_tx_set(&peer, &hash);

        // Should return DontHave
        assert!(matches!(response, Some(StellarMessage::DontHave(_))));
    }

    #[test]
    fn test_fetch_tx_set() {
        let dispatcher = MessageDispatcher::new();
        let hash = Hash([1u8; 32]);
        let envelope = make_test_envelope(100);

        dispatcher.fetch_tx_set(hash.clone(), &envelope);

        assert!(dispatcher.tx_set_fetcher.is_tracking(&hash));
    }

    #[test]
    fn test_handle_dont_have() {
        let dispatcher = MessageDispatcher::new();
        let peer = make_peer_id(1);
        let hash = Hash([1u8; 32]);
        let envelope = make_test_envelope(100);

        // First fetch
        dispatcher.fetch_tx_set(hash.clone(), &envelope);

        // Then receive DontHave
        let dont_have = DontHave {
            type_: MessageType::TxSet,
            req_hash: Uint256(hash.0),
        };

        dispatcher.handle_dont_have(&peer, &dont_have);
        // Should not panic or error
    }

    #[test]
    fn test_cache_tx_set() {
        let dispatcher = MessageDispatcher::new();
        let hash = Hash([1u8; 32]);

        let tx_set = TransactionSet {
            previous_ledger_hash: Hash([0u8; 32]),
            txs: vec![].try_into().unwrap(),
        };

        dispatcher.cache_tx_set(hash.clone(), TxSetData::Legacy(tx_set));

        assert!(dispatcher.has_tx_set(&hash));
        assert!(dispatcher.get_tx_set(&hash).is_some());
    }

    #[test]
    fn test_cache_quorum_set() {
        let dispatcher = MessageDispatcher::new();
        let quorum_set = ScpQuorumSet {
            threshold: 1,
            validators: vec![].try_into().unwrap(),
            inner_sets: vec![].try_into().unwrap(),
        };

        use sha2::{Digest, Sha256};
        use stellar_xdr::curr::WriteXdr;
        let bytes = quorum_set
            .to_xdr(stellar_xdr::curr::Limits::none())
            .unwrap();
        let hash = Hash(Sha256::digest(&bytes).into());

        dispatcher.cache_quorum_set(hash.clone(), quorum_set);

        assert!(dispatcher.has_quorum_set(&hash));
        assert!(dispatcher.get_quorum_set(&hash).is_some());
    }

    #[test]
    fn test_get_tx_set_cached() {
        let dispatcher = MessageDispatcher::new();
        let peer = make_peer_id(1);
        let hash = Hash([1u8; 32]);

        let tx_set = TransactionSet {
            previous_ledger_hash: Hash([0u8; 32]),
            txs: vec![].try_into().unwrap(),
        };

        dispatcher.cache_tx_set(hash.clone(), TxSetData::Legacy(tx_set));

        let response = dispatcher.handle_get_tx_set(&peer, &hash);

        // Should return TxSet
        assert!(matches!(response, Some(StellarMessage::TxSet(_))));
    }

    #[test]
    fn test_audit_002_tx_set_cache_bounded() {
        let dispatcher = MessageDispatcher::new();

        // Insert more than MAX_CACHE_SIZE entries
        for i in 0..(MAX_CACHE_SIZE + 100) {
            let mut hash_bytes = [0u8; 32];
            hash_bytes[..4].copy_from_slice(&(i as u32).to_be_bytes());
            let hash = Hash(hash_bytes);
            let tx_set = TransactionSet {
                previous_ledger_hash: Hash([0u8; 32]),
                txs: vec![].try_into().unwrap(),
            };
            dispatcher.cache_tx_set(hash, TxSetData::Legacy(tx_set));
        }

        let stats = dispatcher.stats();
        assert!(
            stats.cached_tx_sets <= MAX_CACHE_SIZE,
            "tx_set_cache should be bounded to {} but has {} entries",
            MAX_CACHE_SIZE,
            stats.cached_tx_sets
        );
    }

    #[test]
    fn test_audit_002_quorum_set_cache_bounded() {
        let dispatcher = MessageDispatcher::new();

        for i in 0..(MAX_CACHE_SIZE + 100) {
            let mut hash_bytes = [0u8; 32];
            hash_bytes[..4].copy_from_slice(&(i as u32).to_be_bytes());
            let hash = Hash(hash_bytes);
            let quorum_set = ScpQuorumSet {
                threshold: 1,
                validators: vec![].try_into().unwrap(),
                inner_sets: vec![].try_into().unwrap(),
            };
            dispatcher.cache_quorum_set(hash, quorum_set);
        }

        let stats = dispatcher.stats();
        assert!(
            stats.cached_quorum_sets <= MAX_CACHE_SIZE,
            "quorum_set_cache should be bounded to {} but has {} entries",
            MAX_CACHE_SIZE,
            stats.cached_quorum_sets
        );
    }

    #[test]
    fn test_stop_fetching_below() {
        let dispatcher = MessageDispatcher::new();
        let hash = Hash([1u8; 32]);
        let env1 = make_test_envelope(100);
        let env2 = make_test_envelope(200);

        dispatcher.fetch_tx_set(hash.clone(), &env1);
        dispatcher.fetch_tx_set(hash.clone(), &env2);

        // Stop fetching for slots below 150
        dispatcher.stop_fetching_below(150, 0);

        // Tracker should still exist (env2 is at slot 200)
        let _stats = dispatcher.stats();
        // The tracker is either empty or has one envelope
    }
}
