//! Transaction demand manager for pull-mode flooding.
//!
//! This module implements the TxDemandsManager from stellar-core, which handles
//! demand scheduling with retry logic and responds to incoming demands.
//!
//! # Overview
//!
//! In pull-mode flooding, peers advertise transaction hashes they have, and other
//! peers "demand" (request) the actual transactions. This manager:
//!
//! - Schedules demands to peers based on received advertisements
//! - Tracks demand history per transaction to avoid duplicate requests
//! - Implements linear backoff for retry attempts
//! - Records pull latency metrics
//! - Responds to incoming FloodDemand messages
//!
//! # Demand Lifecycle
//!
//! 1. Peer sends FloodAdvert with transaction hashes
//! 2. TxAdverts queues the hashes for processing
//! 3. TxDemandsManager periodically pops hashes and creates FloodDemand messages
//! 4. If no response, retries with linear backoff (up to MAX_RETRY_COUNT)
//! 5. When transaction received, records pull latency
//! 6. Old demand records are cleaned up after MAX_RETENTION

use crate::PeerId;
use parking_lot::RwLock;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use stellar_xdr::curr::{FloodDemand, Hash, TransactionEnvelope};
use tracing::{debug, trace};

/// Maximum number of retry attempts before giving up on a transaction.
pub const MAX_RETRY_COUNT: u32 = 15;

/// Maximum delay between demand retries (2 seconds).
const MAX_DELAY_DEMAND: Duration = Duration::from_secs(2);

/// Maximum number of hashes in a demand message.
pub const TX_DEMAND_VECTOR_MAX_SIZE: usize = 1000;

/// Default demand period (milliseconds).
/// Spec: OVERLAY_SPEC §6.3.2 — DEMAND_PERIOD = 200ms.
const DEFAULT_DEMAND_PERIOD_MS: u64 = 200;

/// Default backoff delay per attempt (milliseconds).
/// Spec: OVERLAY_SPEC §6.3.2 — BACKOFF_DELAY = 500ms.
const DEFAULT_BACKOFF_DELAY_MS: u64 = 500;

/// Configuration for transaction demands.
#[derive(Debug, Clone)]
pub struct TxDemandsConfig {
    /// Period between demand cycles.
    pub demand_period: Duration,
    /// Backoff delay per retry attempt.
    pub backoff_delay: Duration,
    /// Maximum number of hashes per demand message.
    pub max_demand_size: usize,
    /// Maximum number of retry attempts.
    pub max_retry_count: u32,
}

impl Default for TxDemandsConfig {
    fn default() -> Self {
        Self {
            demand_period: Duration::from_millis(DEFAULT_DEMAND_PERIOD_MS),
            backoff_delay: Duration::from_millis(DEFAULT_BACKOFF_DELAY_MS),
            max_demand_size: TX_DEMAND_VECTOR_MAX_SIZE,
            max_retry_count: MAX_RETRY_COUNT,
        }
    }
}

/// Status of whether to demand a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemandStatus {
    /// Demand the transaction now.
    Demand,
    /// Retry later (backoff timer not expired).
    RetryLater,
    /// Discard (already have, banned, or max retries exceeded).
    Discard,
}

/// History of demanding a specific transaction.
#[derive(Debug, Clone)]
struct DemandHistory {
    /// When we first demanded this transaction.
    first_demanded: Instant,
    /// When we last demanded this transaction.
    last_demanded: Instant,
    /// Map of peer IDs to when we demanded from them.
    peers: HashMap<PeerId, Instant>,
    /// Whether we've recorded the pull latency.
    latency_recorded: bool,
}

impl DemandHistory {
    fn new(now: Instant) -> Self {
        Self {
            first_demanded: now,
            last_demanded: now,
            peers: HashMap::new(),
            latency_recorded: false,
        }
    }
}

/// Result of processing demands for a peer.
#[derive(Debug)]
pub struct PeerDemandResult {
    /// Hashes to demand from this peer.
    pub to_demand: Vec<Hash>,
    /// Hashes to retry later.
    pub to_retry: Vec<Hash>,
}

/// Callback for checking if a transaction is known/banned.
pub type TxStatusFn = Box<dyn Fn(&Hash) -> TxKnownStatus + Send + Sync>;

/// Status of a transaction hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxKnownStatus {
    /// Transaction is unknown, should demand it.
    Unknown,
    /// Transaction is already known/received.
    Known,
    /// Transaction is banned.
    Banned,
}

/// Callback for fetching a transaction by hash.
pub type GetTxFn = Box<dyn Fn(&Hash) -> Option<TransactionEnvelope> + Send + Sync>;

/// Internal state protected by lock.
struct TxDemandsState {
    /// Map of transaction hash to demand history.
    demand_history: HashMap<Hash, DemandHistory>,
    /// Queue of pending demands (for ordering/cleanup).
    pending_demands: VecDeque<Hash>,
    /// Whether the manager is running.
    running: bool,
}

/// Transaction demand manager.
///
/// Manages demand scheduling with retry logic and tracks pull latency.
pub struct TxDemandsManager {
    /// Configuration.
    config: TxDemandsConfig,
    /// Protected state.
    state: RwLock<TxDemandsState>,
    /// Callback to check transaction status.
    tx_status_fn: RwLock<Option<TxStatusFn>>,
    /// Callback to get transaction by hash.
    get_tx_fn: RwLock<Option<GetTxFn>>,
}

impl TxDemandsManager {
    /// Create a new demand manager with the given configuration.
    pub fn new(config: TxDemandsConfig) -> Self {
        Self {
            config,
            state: RwLock::new(TxDemandsState {
                demand_history: HashMap::new(),
                pending_demands: VecDeque::new(),
                running: false,
            }),
            tx_status_fn: RwLock::new(None),
            get_tx_fn: RwLock::new(None),
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(TxDemandsConfig::default())
    }

    /// Set the callback for checking transaction status.
    pub fn set_tx_status_fn(&self, f: TxStatusFn) {
        let mut cb = self.tx_status_fn.write();
        *cb = Some(f);
    }

    /// Set the callback for fetching transactions.
    pub fn set_get_tx_fn(&self, f: GetTxFn) {
        let mut cb = self.get_tx_fn.write();
        *cb = Some(f);
    }

    /// Start the demand manager.
    pub fn start(&self) {
        let mut state = self.state.write();
        state.running = true;
        debug!("TxDemandsManager started");
    }

    /// Stop the demand manager.
    pub fn shutdown(&self) {
        let mut state = self.state.write();
        state.running = false;
        debug!("TxDemandsManager shutdown");
    }

    /// Check if the manager is running.
    pub fn is_running(&self) -> bool {
        let state = self.state.read();
        state.running
    }

    /// Compute the retry delay based on number of attempts.
    ///
    /// Uses linear backoff: delay = num_attempts * backoff_delay,
    /// capped at MAX_DELAY_DEMAND.
    pub fn retry_delay(&self, num_attempts: u32) -> Duration {
        let delay = self.config.backoff_delay * num_attempts;
        delay.min(MAX_DELAY_DEMAND)
    }

    /// Determine whether to demand a transaction from a peer.
    pub fn demand_status(&self, tx_hash: &Hash, peer_id: &PeerId) -> DemandStatus {
        // Check transaction status via callback
        let status_fn = self.tx_status_fn.read();
        if let Some(ref check_status) = *status_fn {
            match check_status(tx_hash) {
                TxKnownStatus::Known | TxKnownStatus::Banned => {
                    return DemandStatus::Discard;
                }
                TxKnownStatus::Unknown => {}
            }
        }

        let state = self.state.read();
        let now = Instant::now();

        match state.demand_history.get(tx_hash) {
            None => {
                // Never demanded this transaction
                DemandStatus::Demand
            }
            Some(history) => {
                // Check if we've already demanded from this peer
                if history.peers.contains_key(peer_id) {
                    return DemandStatus::Discard;
                }

                let num_demanded = history.peers.len() as u32;

                if num_demanded < self.config.max_retry_count {
                    // Check if backoff timer has expired
                    let delay = self.retry_delay(num_demanded);
                    if now.duration_since(history.last_demanded) >= delay {
                        DemandStatus::Demand
                    } else {
                        DemandStatus::RetryLater
                    }
                } else {
                    // Max retries exceeded
                    DemandStatus::Discard
                }
            }
        }
    }

    /// Process a batch of advertised hashes from a peer.
    ///
    /// Returns which hashes to demand and which to retry later.
    pub fn process_adverts(
        &self,
        hashes: &[Hash],
        peer_id: &PeerId,
        max_demand: usize,
    ) -> PeerDemandResult {
        let mut to_demand = Vec::new();
        let mut to_retry = Vec::new();

        for hash in hashes {
            if to_demand.len() >= max_demand {
                // Already have enough demands, retry the rest
                to_retry.push(hash.clone());
                continue;
            }

            match self.demand_status(hash, peer_id) {
                DemandStatus::Demand => {
                    to_demand.push(hash.clone());
                }
                DemandStatus::RetryLater => {
                    to_retry.push(hash.clone());
                }
                DemandStatus::Discard => {
                    // Don't add to either list
                }
            }
        }

        PeerDemandResult {
            to_demand,
            to_retry,
        }
    }

    /// Record that we are demanding transactions from a peer.
    ///
    /// Call this after sending a FloodDemand message.
    pub fn record_demands(&self, hashes: &[Hash], peer_id: &PeerId) {
        let mut state = self.state.write();
        let now = Instant::now();

        for hash in hashes {
            // Check if this is a new hash
            let is_new = !state.demand_history.contains_key(hash);
            if is_new {
                // First time demanding this hash
                state.pending_demands.push_back(hash.clone());
                state
                    .demand_history
                    .insert(hash.clone(), DemandHistory::new(now));
            }

            // Get the history entry (now guaranteed to exist)
            let history = state.demand_history.get_mut(hash).unwrap();

            // Record this peer
            history.peers.insert(peer_id.clone(), now);
            history.last_demanded = now;

            let is_retry = history.peers.len() > 1;
            if is_retry {
                trace!(
                    "Retrying demand for tx {:?}, attempt {} from peer {}",
                    hex::encode(&hash.0[..4]),
                    history.peers.len(),
                    peer_id
                );
            } else {
                trace!(
                    "Demanding tx {:?} from peer {}",
                    hex::encode(&hash.0[..4]),
                    peer_id
                );
            }
        }
    }

    /// Record that we successfully received a transaction.
    ///
    /// Returns the pull latency if this was the first time receiving it.
    pub fn record_tx_received(&self, tx_hash: &Hash, peer_id: &PeerId) -> Option<TxPullLatency> {
        let mut state = self.state.write();
        let now = Instant::now();

        let history = state.demand_history.get_mut(tx_hash)?;

        let mut result = None;

        // Record end-to-end pull time (only once)
        if !history.latency_recorded {
            let total_latency = now.duration_since(history.first_demanded);
            let num_peers_asked = history.peers.len();

            history.latency_recorded = true;

            debug!(
                "Pulled transaction {:?} in {:?}, asked {} peers",
                hex::encode(&tx_hash.0[..4]),
                total_latency,
                num_peers_asked
            );

            // Record peer-specific latency if we demanded from this peer
            let peer_latency = history
                .peers
                .get(peer_id)
                .map(|&demanded_at| now.duration_since(demanded_at));

            result = Some(TxPullLatency {
                total_latency,
                peer_latency,
                peers_asked: num_peers_asked,
            });
        }

        result
    }

    /// Clean up old demand records.
    ///
    /// Returns the number of abandoned demands (never received).
    pub fn cleanup_old_demands(&self) -> CleanupResult {
        let mut state = self.state.write();
        let now = Instant::now();

        // Maximum retention time: 2 * MAX_RETRY_COUNT * MAX_DELAY_DEMAND
        let max_retention = MAX_DELAY_DEMAND * self.config.max_retry_count * 2;

        let mut abandoned = 0;
        let mut cleaned = 0;

        while let Some(hash) = state.pending_demands.front().cloned() {
            if let Some(history) = state.demand_history.get(&hash) {
                if now.duration_since(history.first_demanded) >= max_retention {
                    if !history.latency_recorded {
                        // We never received this transaction
                        abandoned += 1;
                        trace!(
                            "Abandoned demand for tx {:?} after {} attempts",
                            hex::encode(&hash.0[..4]),
                            history.peers.len()
                        );
                    }
                    state.demand_history.remove(&hash);
                    state.pending_demands.pop_front();
                    cleaned += 1;
                } else {
                    // Oldest demand is not old enough yet
                    break;
                }
            } else {
                // Hash in queue but not in map - shouldn't happen, clean up
                state.pending_demands.pop_front();
            }
        }

        CleanupResult { abandoned, cleaned }
    }

    /// Handle an incoming FloodDemand message.
    ///
    /// Returns transactions to send back to the peer.
    pub fn recv_demand(&self, demand: &FloodDemand) -> Vec<TransactionEnvelope> {
        let get_tx = self.get_tx_fn.read();
        let Some(ref get_tx_fn) = *get_tx else {
            return Vec::new();
        };

        let mut result = Vec::new();
        let mut fulfilled = 0;
        let mut not_found = 0;

        for hash in demand.tx_hashes.iter() {
            if let Some(tx) = get_tx_fn(hash) {
                result.push(tx);
                fulfilled += 1;
            } else {
                not_found += 1;
            }
        }

        if fulfilled > 0 || not_found > 0 {
            trace!("Fulfilled {} demands, {} not found", fulfilled, not_found);
        }

        result
    }

    /// Get statistics about the demand manager.
    pub fn stats(&self) -> TxDemandsStats {
        let state = self.state.read();
        TxDemandsStats {
            pending_demands: state.pending_demands.len(),
            demand_history_size: state.demand_history.len(),
            running: state.running,
        }
    }

    /// Get the number of pending demand records.
    pub fn pending_count(&self) -> usize {
        let state = self.state.read();
        state.pending_demands.len()
    }
}

impl Default for TxDemandsManager {
    fn default() -> Self {
        Self::with_defaults()
    }
}

/// Pull latency information for a transaction.
#[derive(Debug, Clone)]
pub struct TxPullLatency {
    /// Total time from first demand to receipt.
    pub total_latency: Duration,
    /// Time from demanding from this specific peer to receipt (if applicable).
    pub peer_latency: Option<Duration>,
    /// Number of peers we asked.
    pub peers_asked: usize,
}

/// Result of cleaning up old demands.
#[derive(Debug, Clone, Default)]
pub struct CleanupResult {
    /// Number of demands that were abandoned (never fulfilled).
    pub abandoned: usize,
    /// Total number of records cleaned up.
    pub cleaned: usize,
}

/// Statistics about the demand manager.
#[derive(Debug, Clone)]
pub struct TxDemandsStats {
    /// Number of pending demand records.
    pub pending_demands: usize,
    /// Size of demand history map.
    pub demand_history_size: usize,
    /// Whether the manager is running.
    pub running: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread::sleep;

    fn make_hash(id: u8) -> Hash {
        Hash([id; 32])
    }

    fn make_peer_id(id: u8) -> PeerId {
        PeerId::from_bytes([id; 32])
    }

    #[test]
    fn test_demand_manager_creation() {
        let manager = TxDemandsManager::with_defaults();
        assert!(!manager.is_running());
        assert_eq!(manager.pending_count(), 0);
    }

    #[test]
    fn test_start_shutdown() {
        let manager = TxDemandsManager::with_defaults();

        manager.start();
        assert!(manager.is_running());

        manager.shutdown();
        assert!(!manager.is_running());
    }

    #[test]
    fn test_demand_status_unknown_tx() {
        let manager = TxDemandsManager::with_defaults();
        let hash = make_hash(1);
        let peer = make_peer_id(1);

        // Unknown transaction should be demanded
        let status = manager.demand_status(&hash, &peer);
        assert_eq!(status, DemandStatus::Demand);
    }

    #[test]
    fn test_demand_status_known_tx() {
        let manager = TxDemandsManager::with_defaults();
        let hash = make_hash(1);
        let peer = make_peer_id(1);

        // Set up callback that says tx is known
        manager.set_tx_status_fn(Box::new(|_| TxKnownStatus::Known));

        let status = manager.demand_status(&hash, &peer);
        assert_eq!(status, DemandStatus::Discard);
    }

    #[test]
    fn test_demand_status_banned_tx() {
        let manager = TxDemandsManager::with_defaults();
        let hash = make_hash(1);
        let peer = make_peer_id(1);

        // Set up callback that says tx is banned
        manager.set_tx_status_fn(Box::new(|_| TxKnownStatus::Banned));

        let status = manager.demand_status(&hash, &peer);
        assert_eq!(status, DemandStatus::Discard);
    }

    #[test]
    fn test_demand_status_already_demanded_from_peer() {
        let manager = TxDemandsManager::with_defaults();
        let hash = make_hash(1);
        let peer = make_peer_id(1);

        // Record a demand
        manager.record_demands(std::slice::from_ref(&hash), &peer);

        // Should discard since we already demanded from this peer
        let status = manager.demand_status(&hash, &peer);
        assert_eq!(status, DemandStatus::Discard);
    }

    #[test]
    fn test_demand_status_retry_from_different_peer() {
        let mut config = TxDemandsConfig::default();
        config.backoff_delay = Duration::from_millis(1); // Very short for testing

        let manager = TxDemandsManager::new(config);
        let hash = make_hash(1);
        let peer1 = make_peer_id(1);
        let peer2 = make_peer_id(2);

        // Record demand from peer1
        manager.record_demands(std::slice::from_ref(&hash), &peer1);

        // Immediately should be retry_later (backoff not expired)
        let status = manager.demand_status(&hash, &peer2);
        assert_eq!(status, DemandStatus::RetryLater);

        // Wait for backoff
        sleep(Duration::from_millis(5));

        // Now should be able to demand from peer2
        let status = manager.demand_status(&hash, &peer2);
        assert_eq!(status, DemandStatus::Demand);
    }

    #[test]
    fn test_demand_status_max_retries() {
        let mut config = TxDemandsConfig::default();
        config.max_retry_count = 2;
        config.backoff_delay = Duration::from_millis(1);

        let manager = TxDemandsManager::new(config);
        let hash = make_hash(1);

        // Demand from max_retry_count peers
        for i in 0..2 {
            let peer = make_peer_id(i);
            manager.record_demands(std::slice::from_ref(&hash), &peer);
            sleep(Duration::from_millis(5)); // Wait for backoff
        }

        // Next peer should be discarded (max retries)
        let peer3 = make_peer_id(3);
        let status = manager.demand_status(&hash, &peer3);
        assert_eq!(status, DemandStatus::Discard);
    }

    #[test]
    fn test_process_adverts() {
        let manager = TxDemandsManager::with_defaults();
        let peer = make_peer_id(1);

        let hashes = vec![make_hash(1), make_hash(2), make_hash(3)];

        let result = manager.process_adverts(&hashes, &peer, 10);

        assert_eq!(result.to_demand.len(), 3);
        assert_eq!(result.to_retry.len(), 0);
    }

    #[test]
    fn test_process_adverts_with_limit() {
        let manager = TxDemandsManager::with_defaults();
        let peer = make_peer_id(1);

        let hashes = vec![make_hash(1), make_hash(2), make_hash(3)];

        // Limit to 2 demands
        let result = manager.process_adverts(&hashes, &peer, 2);

        assert_eq!(result.to_demand.len(), 2);
        assert_eq!(result.to_retry.len(), 1);
    }

    #[test]
    fn test_record_tx_received() {
        let manager = TxDemandsManager::with_defaults();
        let hash = make_hash(1);
        let peer = make_peer_id(1);

        // Record demand
        manager.record_demands(std::slice::from_ref(&hash), &peer);

        // Record receipt
        let latency = manager.record_tx_received(&hash, &peer);

        assert!(latency.is_some());
        let latency = latency.unwrap();
        assert_eq!(latency.peers_asked, 1);
        assert!(latency.peer_latency.is_some());

        // Second receipt should return None (already recorded)
        let latency2 = manager.record_tx_received(&hash, &peer);
        assert!(latency2.is_none());
    }

    #[test]
    fn test_cleanup_old_demands() {
        let mut config = TxDemandsConfig::default();
        config.max_retry_count = 1;

        let manager = TxDemandsManager::new(config);
        let hash = make_hash(1);
        let peer = make_peer_id(1);

        // Record demand
        manager.record_demands(std::slice::from_ref(&hash), &peer);

        // Cleanup should not remove yet (not old enough)
        let result = manager.cleanup_old_demands();
        assert_eq!(result.cleaned, 0);

        // Stats should show 1 pending
        let stats = manager.stats();
        assert_eq!(stats.pending_demands, 1);
    }

    #[test]
    fn test_recv_demand() {
        let manager = TxDemandsManager::with_defaults();

        // Set up get_tx callback
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        manager.set_get_tx_fn(Box::new(move |hash| {
            call_count_clone.fetch_add(1, Ordering::SeqCst);
            if hash.0[0] == 1 {
                // Return a dummy transaction for hash 1
                Some(TransactionEnvelope::TxV0(
                    stellar_xdr::curr::TransactionV0Envelope {
                        tx: stellar_xdr::curr::TransactionV0 {
                            source_account_ed25519: stellar_xdr::curr::Uint256([0; 32]),
                            fee: 100,
                            seq_num: stellar_xdr::curr::SequenceNumber(1),
                            time_bounds: None,
                            memo: stellar_xdr::curr::Memo::None,
                            operations: vec![].try_into().unwrap(),
                            ext: stellar_xdr::curr::TransactionV0Ext::V0,
                        },
                        signatures: vec![].try_into().unwrap(),
                    },
                ))
            } else {
                None
            }
        }));

        let demand = FloodDemand {
            tx_hashes: vec![make_hash(1), make_hash(2)].try_into().unwrap(),
        };

        let result = manager.recv_demand(&demand);

        assert_eq!(result.len(), 1); // Only hash 1 found
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_retry_delay() {
        let mut config = TxDemandsConfig::default();
        config.backoff_delay = Duration::from_millis(100);

        let manager = TxDemandsManager::new(config);

        // Linear backoff
        assert_eq!(manager.retry_delay(0), Duration::from_millis(0));
        assert_eq!(manager.retry_delay(1), Duration::from_millis(100));
        assert_eq!(manager.retry_delay(5), Duration::from_millis(500));
        assert_eq!(manager.retry_delay(10), Duration::from_millis(1000));

        // Capped at MAX_DELAY_DEMAND
        assert_eq!(manager.retry_delay(100), MAX_DELAY_DEMAND);
    }

    #[test]
    fn test_stats() {
        let manager = TxDemandsManager::with_defaults();
        let hash = make_hash(1);
        let peer = make_peer_id(1);

        let stats = manager.stats();
        assert_eq!(stats.pending_demands, 0);
        assert_eq!(stats.demand_history_size, 0);
        assert!(!stats.running);

        manager.start();
        manager.record_demands(&[hash], &peer);

        let stats = manager.stats();
        assert_eq!(stats.pending_demands, 1);
        assert_eq!(stats.demand_history_size, 1);
        assert!(stats.running);
    }

    // ── OVERLAY_SPEC §6.3.2: Demand timing constants ─────────────────

    #[test]
    fn test_default_demand_period_is_200ms() {
        let config = TxDemandsConfig::default();
        assert_eq!(
            config.demand_period,
            Duration::from_millis(200),
            "OVERLAY_SPEC §6.3.2: DEMAND_PERIOD must be 200ms"
        );
    }

    #[test]
    fn test_default_backoff_delay_is_500ms() {
        let config = TxDemandsConfig::default();
        assert_eq!(
            config.backoff_delay,
            Duration::from_millis(500),
            "OVERLAY_SPEC §6.3.2: BACKOFF_DELAY must be 500ms"
        );
    }

    #[test]
    fn test_default_max_demand_size() {
        let config = TxDemandsConfig::default();
        assert_eq!(config.max_demand_size, 1000);
        assert_eq!(TX_DEMAND_VECTOR_MAX_SIZE, 1000);
    }

    #[test]
    fn test_default_max_retry_count() {
        let config = TxDemandsConfig::default();
        assert_eq!(config.max_retry_count, 15);
        assert_eq!(MAX_RETRY_COUNT, 15);
    }
}
