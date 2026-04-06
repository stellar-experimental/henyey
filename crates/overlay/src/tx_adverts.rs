//! Transaction advertisement management.
//!
//! This module implements the TxAdverts class from stellar-core, which handles
//! batching and broadcasting of transaction hash advertisements in pull-mode flooding.
//!
//! # Overview
//!
//! - Incoming adverts are queued for demanding
//! - Outgoing adverts are batched and periodically flushed
//! - A history cache tracks seen adverts to avoid duplicates
//!
//! # Flow
//!
//! 1. When we receive a transaction, we call `queue_outgoing_advert` to advertise it
//! 2. Adverts are batched until either the batch is full or a timer expires
//! 3. When we receive a FloodAdvert, we call `queue_incoming_advert` to process it
//! 4. We pop adverts from the incoming queue to demand transactions

use parking_lot::RwLock;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use stellar_xdr::curr::{FloodAdvert, Hash, StellarMessage};
use tracing::trace;

/// Default size of the advert history cache.
const ADVERT_CACHE_SIZE: usize = 50000;

/// Maximum number of hashes in an advert message.
pub const TX_ADVERT_VECTOR_MAX_SIZE: usize = 1000;

/// Default advert flush period (milliseconds).
const DEFAULT_ADVERT_PERIOD_MS: u64 = 100;

/// Configuration for transaction advertisements.
#[derive(Debug, Clone)]
pub struct TxAdvertsConfig {
    /// Period for flushing outgoing adverts.
    pub advert_period: Duration,
    /// Maximum size of outgoing advert batch.
    pub max_advert_size: usize,
    /// Maximum size of the advert history cache.
    pub history_cache_size: usize,
    /// Maximum operations to track (limits incoming queue size).
    pub max_ops: usize,
}

impl Default for TxAdvertsConfig {
    fn default() -> Self {
        Self {
            advert_period: Duration::from_millis(DEFAULT_ADVERT_PERIOD_MS),
            max_advert_size: TX_ADVERT_VECTOR_MAX_SIZE,
            history_cache_size: ADVERT_CACHE_SIZE,
            max_ops: 10000,
        }
    }
}

/// Entry in the advert history cache.
#[derive(Debug, Clone)]
struct AdvertHistoryEntry {
    /// Ledger sequence when this advert was seen.
    ledger_seq: u32,
}

/// Callback for sending FloodAdvert messages.
pub type SendAdvertFn = Box<dyn Fn(StellarMessage) + Send + Sync>;

/// Internal state protected by lock.
struct TxAdvertsState {
    /// Incoming transaction hashes to demand.
    incoming_tx_hashes: VecDeque<Hash>,
    /// Transaction hashes to retry demanding.
    tx_hashes_to_retry: VecDeque<Hash>,
    /// Cache of seen hashes (hash -> ledger sequence).
    advert_history: HashMap<Hash, AdvertHistoryEntry>,
    /// Outgoing transaction hashes to advertise.
    outgoing_tx_hashes: Vec<Hash>,
    /// When the current outgoing batch started.
    batch_start_time: Option<Instant>,
}

/// Transaction advertisement manager.
///
/// Handles batching and queuing of transaction hash advertisements
/// for pull-mode flooding.
pub struct TxAdverts {
    /// Configuration.
    config: TxAdvertsConfig,
    /// Protected state.
    state: RwLock<TxAdvertsState>,
    /// Callback for sending adverts.
    send_callback: RwLock<Option<SendAdvertFn>>,
}

impl TxAdverts {
    /// Create a new TxAdverts manager.
    pub fn new(config: TxAdvertsConfig) -> Self {
        Self {
            config,
            state: RwLock::new(TxAdvertsState {
                incoming_tx_hashes: VecDeque::new(),
                tx_hashes_to_retry: VecDeque::new(),
                advert_history: HashMap::with_capacity(ADVERT_CACHE_SIZE),
                outgoing_tx_hashes: Vec::new(),
                batch_start_time: None,
            }),
            send_callback: RwLock::new(None),
        }
    }

    /// Set the callback for sending FloodAdvert messages.
    pub fn set_send_callback(&self, callback: SendAdvertFn) {
        let mut cb = self.send_callback.write();
        *cb = Some(callback);
    }

    /// Total transaction hashes to process including demand retries.
    pub fn size(&self) -> usize {
        let state = self.state.read();
        state.incoming_tx_hashes.len() + state.tx_hashes_to_retry.len()
    }

    /// Check if there are adverts to process.
    pub fn has_adverts(&self) -> bool {
        self.size() > 0
    }

    /// Pop the next advert hash to process.
    ///
    /// Returns None if there are no adverts to process.
    pub fn pop_incoming_advert(&self) -> Option<Hash> {
        let mut state = self.state.write();

        // Retry queue has priority
        if let Some(hash) = state.tx_hashes_to_retry.pop_front() {
            return Some(hash);
        }

        state.incoming_tx_hashes.pop_front()
    }

    /// Queue up a transaction hash to advertise to neighbours.
    pub fn queue_outgoing_advert(&self, tx_hash: Hash) {
        let should_flush = {
            let mut state = self.state.write();

            if state.outgoing_tx_hashes.is_empty() {
                state.batch_start_time = Some(Instant::now());
            }

            state.outgoing_tx_hashes.push(tx_hash);

            state.outgoing_tx_hashes.len() >= self.config.max_advert_size
        };

        if should_flush {
            self.flush_advert();
        }
    }

    /// Queue up transaction hashes from a neighbour to try demanding.
    // SECURITY: advert queue bounded by per-peer flow control window (MAX_HASH_COUNT)
    pub fn queue_incoming_advert(&self, tx_hashes: &[Hash], ledger_seq: u32) {
        let mut state = self.state.write();

        // Remember all hashes in history
        for hash in tx_hashes {
            state
                .advert_history
                .insert(hash.clone(), AdvertHistoryEntry { ledger_seq });
        }

        // Trim history if too large (simple LRU-like eviction)
        while state.advert_history.len() > self.config.history_cache_size {
            // Remove oldest entry (this is approximate LRU)
            if let Some(key) = state.advert_history.keys().next().cloned() {
                state.advert_history.remove(&key);
            }
        }

        // Add hashes to incoming queue, respecting limit
        let limit = self.config.max_ops;
        let start_idx = if tx_hashes.len() > limit {
            tx_hashes.len() - limit
        } else {
            0
        };

        for hash in &tx_hashes[start_idx..] {
            state.incoming_tx_hashes.push_back(hash.clone());
        }

        // Trim incoming queue if over limit
        let total_size = state.incoming_tx_hashes.len() + state.tx_hashes_to_retry.len();
        if total_size > limit {
            let to_remove = total_size - limit;
            for _ in 0..to_remove {
                // Pop from incoming first, then retry
                if state.incoming_tx_hashes.pop_front().is_none() {
                    state.tx_hashes_to_retry.pop_front();
                }
            }
        }
    }

    /// Queue up transaction hashes to retry demanding.
    pub fn retry_incoming_advert(&self, hashes: Vec<Hash>) {
        let mut state = self.state.write();

        for hash in hashes {
            state.tx_hashes_to_retry.push_back(hash);
        }

        // Trim if over limit
        let total_size = state.incoming_tx_hashes.len() + state.tx_hashes_to_retry.len();
        let limit = self.config.max_ops;
        if total_size > limit {
            let to_remove = total_size - limit;
            for _ in 0..to_remove {
                if state.incoming_tx_hashes.pop_front().is_none() {
                    state.tx_hashes_to_retry.pop_front();
                }
            }
        }
    }

    /// Check if we've seen this advert before.
    pub fn seen_advert(&self, hash: &Hash) -> bool {
        let state = self.state.read();
        state.advert_history.contains_key(hash)
    }

    /// Clear advert history for ledgers below the given sequence.
    pub fn clear_below(&self, ledger_seq: u32) {
        let mut state = self.state.write();
        state
            .advert_history
            .retain(|_, entry| entry.ledger_seq >= ledger_seq);
    }

    /// Flush outgoing adverts immediately.
    pub fn flush_advert(&self) {
        let outgoing = {
            let mut state = self.state.write();
            if state.outgoing_tx_hashes.is_empty() {
                return;
            }

            let hashes = std::mem::take(&mut state.outgoing_tx_hashes);
            state.batch_start_time = None;
            hashes
        };

        trace!("Flushing {} outgoing adverts", outgoing.len());

        // Send via callback
        let callback = self.send_callback.read();
        if let Some(ref send) = *callback {
            let msg = StellarMessage::FloodAdvert(FloodAdvert {
                tx_hashes: outgoing.try_into().unwrap_or_default(),
            });
            send(msg);
        }
    }

    /// Check if it's time to flush adverts based on timer.
    ///
    /// Returns true if adverts were flushed.
    pub fn maybe_flush_on_timer(&self) -> bool {
        let should_flush = {
            let state = self.state.read();
            if let Some(start_time) = state.batch_start_time {
                start_time.elapsed() >= self.config.advert_period
            } else {
                false
            }
        };

        if should_flush {
            self.flush_advert();
            true
        } else {
            false
        }
    }

    /// Get the number of outgoing adverts waiting to be sent.
    pub fn outgoing_size(&self) -> usize {
        let state = self.state.read();
        state.outgoing_tx_hashes.len()
    }

    /// Get statistics.
    pub fn stats(&self) -> TxAdvertsStats {
        let state = self.state.read();
        TxAdvertsStats {
            incoming_queue_size: state.incoming_tx_hashes.len(),
            retry_queue_size: state.tx_hashes_to_retry.len(),
            outgoing_queue_size: state.outgoing_tx_hashes.len(),
            history_size: state.advert_history.len(),
        }
    }

    /// Shutdown - flush any remaining adverts.
    pub fn shutdown(&self) {
        self.flush_advert();
    }
}

impl Default for TxAdverts {
    fn default() -> Self {
        Self::new(TxAdvertsConfig::default())
    }
}

/// Statistics about transaction advertisements.
#[derive(Debug, Clone)]
pub struct TxAdvertsStats {
    /// Number of incoming hashes waiting to be demanded.
    pub incoming_queue_size: usize,
    /// Number of hashes waiting to retry demanding.
    pub retry_queue_size: usize,
    /// Number of outgoing hashes waiting to be advertised.
    pub outgoing_queue_size: usize,
    /// Number of hashes in the history cache.
    pub history_size: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn make_hash(id: u8) -> Hash {
        Hash([id; 32])
    }

    #[test]
    fn test_tx_adverts_creation() {
        let adverts = TxAdverts::default();
        assert_eq!(adverts.size(), 0);
        assert!(!adverts.has_adverts());
    }

    #[test]
    fn test_queue_incoming_advert() {
        let adverts = TxAdverts::default();
        let hashes = vec![make_hash(1), make_hash(2), make_hash(3)];

        adverts.queue_incoming_advert(&hashes, 100);

        assert_eq!(adverts.size(), 3);
        assert!(adverts.has_adverts());

        // All hashes should be in history
        assert!(adverts.seen_advert(&make_hash(1)));
        assert!(adverts.seen_advert(&make_hash(2)));
        assert!(adverts.seen_advert(&make_hash(3)));
        assert!(!adverts.seen_advert(&make_hash(4)));
    }

    #[test]
    fn test_pop_incoming_advert() {
        let adverts = TxAdverts::default();
        let hashes = vec![make_hash(1), make_hash(2)];

        adverts.queue_incoming_advert(&hashes, 100);

        let h1 = adverts.pop_incoming_advert();
        assert_eq!(h1, Some(make_hash(1)));
        assert_eq!(adverts.size(), 1);

        let h2 = adverts.pop_incoming_advert();
        assert_eq!(h2, Some(make_hash(2)));
        assert_eq!(adverts.size(), 0);

        let h3 = adverts.pop_incoming_advert();
        assert_eq!(h3, None);
    }

    #[test]
    fn test_retry_has_priority() {
        let adverts = TxAdverts::default();

        // Add incoming
        adverts.queue_incoming_advert(&[make_hash(1), make_hash(2)], 100);

        // Add retry
        adverts.retry_incoming_advert(vec![make_hash(10)]);

        // Retry should come first
        let h1 = adverts.pop_incoming_advert();
        assert_eq!(h1, Some(make_hash(10)));

        // Then incoming
        let h2 = adverts.pop_incoming_advert();
        assert_eq!(h2, Some(make_hash(1)));
    }

    #[test]
    fn test_queue_outgoing_advert() {
        let adverts = TxAdverts::default();

        adverts.queue_outgoing_advert(make_hash(1));
        adverts.queue_outgoing_advert(make_hash(2));

        assert_eq!(adverts.outgoing_size(), 2);
    }

    #[test]
    fn test_flush_advert_with_callback() {
        let adverts = TxAdverts::default();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        adverts.set_send_callback(Box::new(move |_msg| {
            call_count_clone.fetch_add(1, Ordering::SeqCst);
        }));

        adverts.queue_outgoing_advert(make_hash(1));
        adverts.queue_outgoing_advert(make_hash(2));
        adverts.flush_advert();

        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        assert_eq!(adverts.outgoing_size(), 0);
    }

    #[test]
    fn test_auto_flush_on_max_size() {
        let mut config = TxAdvertsConfig::default();
        config.max_advert_size = 3;

        let adverts = TxAdverts::new(config);
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        adverts.set_send_callback(Box::new(move |_msg| {
            call_count_clone.fetch_add(1, Ordering::SeqCst);
        }));

        // Queue 3 adverts - should auto-flush
        adverts.queue_outgoing_advert(make_hash(1));
        adverts.queue_outgoing_advert(make_hash(2));
        adverts.queue_outgoing_advert(make_hash(3));

        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        assert_eq!(adverts.outgoing_size(), 0);
    }

    #[test]
    fn test_clear_below() {
        let adverts = TxAdverts::default();

        adverts.queue_incoming_advert(&[make_hash(1)], 100);
        adverts.queue_incoming_advert(&[make_hash(2)], 200);
        adverts.queue_incoming_advert(&[make_hash(3)], 300);

        // Clear entries below ledger 200
        adverts.clear_below(200);

        // Only entries at ledger 200+ should remain in history
        assert!(!adverts.seen_advert(&make_hash(1)));
        assert!(adverts.seen_advert(&make_hash(2)));
        assert!(adverts.seen_advert(&make_hash(3)));
    }

    #[test]
    fn test_queue_limit() {
        let mut config = TxAdvertsConfig::default();
        config.max_ops = 5;

        let adverts = TxAdverts::new(config);

        // Queue more than limit
        let hashes: Vec<Hash> = (0..10).map(|i| make_hash(i)).collect();
        adverts.queue_incoming_advert(&hashes, 100);

        // Should be trimmed to max_ops
        assert_eq!(adverts.size(), 5);
    }
}
