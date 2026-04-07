//! Transaction broadcast management for the transaction queue.
//!
//! This module provides functionality for broadcasting transactions to the network
//! in a controlled manner. It implements:
//!
//! - **Surge pricing order**: Higher-fee transactions are broadcast first
//! - **Rate limiting**: Transactions are broadcast in batches to avoid flooding
//! - **Rebroadcast**: Previously broadcast transactions can be rebroadcast
//! - **Callback integration**: Integrates with async overlay for actual transmission
//!
//! # Broadcast Flow
//!
//! 1. `broadcast(false)` starts a broadcast cycle by scheduling a timer
//! 2. When the timer fires, `broadcast_some()` sends a batch of transactions
//! 3. If more transactions remain, the timer is rescheduled
//! 4. `rebroadcast()` marks all transactions for rebroadcast and starts a cycle
//!
//! # Example
//!
//! ```ignore
//! use henyey_herder::tx_broadcast::{TxBroadcastManager, TxBroadcastHandle};
//!
//! // Create broadcast manager
//! let (handle, manager) = TxBroadcastManager::new(overlay.clone(), limiter.clone());
//!
//! // Spawn the background task
//! tokio::spawn(manager.run());
//!
//! // Start broadcast cycle
//! handle.broadcast().await;
//!
//! // Force rebroadcast of all transactions
//! handle.rebroadcast().await;
//! ```

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, trace};

use stellar_xdr::curr::{Hash, TransactionEnvelope};

/// Default flood period in milliseconds.
pub const DEFAULT_FLOOD_PERIOD_MS: u64 = 100;

/// Data for the AddTransaction command, boxed to avoid large enum variant size difference.
#[derive(Debug)]
pub struct AddTransactionData {
    /// Hash of the transaction.
    pub tx_hash: Hash,
    /// The transaction envelope to broadcast.
    pub envelope: TransactionEnvelope,
}

/// Commands sent to the broadcast manager.
#[derive(Debug)]
pub enum BroadcastCommand {
    /// Start a broadcast cycle (from external trigger).
    Broadcast,
    /// Force rebroadcast all transactions.
    Rebroadcast,
    /// Add a transaction to be broadcast.
    AddTransaction(Box<AddTransactionData>),
    /// Remove a transaction (e.g., after inclusion in ledger).
    RemoveTransaction { tx_hash: Hash },
    /// Mark a transaction as already broadcast (don't rebroadcast).
    MarkBroadcast { tx_hash: Hash },
    /// Set the flood period.
    SetFloodPeriod { period_ms: u64 },
    /// Shutdown the broadcast manager.
    Shutdown,
}

/// Handle for sending commands to the broadcast manager.
#[derive(Clone)]
pub struct TxBroadcastHandle {
    sender: mpsc::Sender<BroadcastCommand>,
}

impl TxBroadcastHandle {
    /// Start a broadcast cycle.
    pub async fn broadcast(&self) {
        let _ = self.sender.send(BroadcastCommand::Broadcast).await;
    }

    /// Force rebroadcast of all transactions.
    pub async fn rebroadcast(&self) {
        let _ = self.sender.send(BroadcastCommand::Rebroadcast).await;
    }

    /// Add a transaction to be broadcast.
    pub async fn add_transaction(&self, tx_hash: Hash, envelope: TransactionEnvelope) {
        let _ = self
            .sender
            .send(BroadcastCommand::AddTransaction(Box::new(
                AddTransactionData { tx_hash, envelope },
            )))
            .await;
    }

    /// Remove a transaction.
    pub async fn remove_transaction(&self, tx_hash: Hash) {
        let _ = self
            .sender
            .send(BroadcastCommand::RemoveTransaction { tx_hash })
            .await;
    }

    /// Mark a transaction as already broadcast.
    pub async fn mark_broadcast(&self, tx_hash: Hash) {
        let _ = self
            .sender
            .send(BroadcastCommand::MarkBroadcast { tx_hash })
            .await;
    }

    /// Set the flood period.
    pub async fn set_flood_period(&self, period_ms: u64) {
        let _ = self
            .sender
            .send(BroadcastCommand::SetFloodPeriod { period_ms })
            .await;
    }

    /// Shutdown the broadcast manager.
    pub async fn shutdown(&self) {
        let _ = self.sender.send(BroadcastCommand::Shutdown).await;
    }

    /// Try to add a transaction (non-blocking).
    pub fn try_add_transaction(&self, tx_hash: Hash, envelope: TransactionEnvelope) -> bool {
        self.sender
            .try_send(BroadcastCommand::AddTransaction(Box::new(
                AddTransactionData { tx_hash, envelope },
            )))
            .is_ok()
    }
}

/// Callback trait for transaction broadcast.
pub trait TxBroadcastCallback: Send + Sync + 'static {
    /// Broadcast a transaction to the network.
    ///
    /// Returns `true` if the broadcast was successful.
    fn broadcast_transaction(&self, envelope: &TransactionEnvelope) -> bool;

    /// Get the maximum number of transactions to broadcast per period.
    fn get_flood_capacity(&self) -> usize;

    /// Get transactions sorted by priority (highest fee first).
    ///
    /// Returns (tx_hash, envelope, already_broadcast).
    fn get_transactions_by_priority(&self) -> Vec<(Hash, TransactionEnvelope, bool)>;
}

/// Pending transaction for broadcast.
#[derive(Debug, Clone)]
struct PendingTx {
    _envelope: TransactionEnvelope,
    broadcast_count: u32,
    last_broadcast: Option<Instant>,
}

/// Broadcast state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BroadcastState {
    /// Not broadcasting, idle.
    Idle,
    /// Waiting for timer to fire.
    Waiting,
    /// Shutdown.
    Shutdown,
}

/// The transaction broadcast manager.
pub struct TxBroadcastManager<C: TxBroadcastCallback> {
    callback: Arc<C>,
    receiver: mpsc::Receiver<BroadcastCommand>,
    /// Transactions pending broadcast.
    pending: std::collections::HashMap<Hash, PendingTx>,
    /// Set of already broadcast transaction hashes.
    broadcast_set: HashSet<Hash>,
    /// Current state.
    state: BroadcastState,
    /// Flood period.
    flood_period: Duration,
    /// When the next broadcast should happen.
    next_broadcast: Option<Instant>,
}

impl<C: TxBroadcastCallback> TxBroadcastManager<C> {
    /// Create a new broadcast manager.
    pub fn new(callback: Arc<C>) -> (TxBroadcastHandle, Self) {
        let (sender, receiver) = mpsc::channel(256);
        let handle = TxBroadcastHandle { sender };
        let manager = Self {
            callback,
            receiver,
            pending: std::collections::HashMap::new(),
            broadcast_set: HashSet::new(),
            state: BroadcastState::Idle,
            flood_period: Duration::from_millis(DEFAULT_FLOOD_PERIOD_MS),
            next_broadcast: None,
        };
        (handle, manager)
    }

    /// Run the broadcast manager.
    pub async fn run(mut self) {
        info!("Transaction broadcast manager started");

        loop {
            let timeout = self.next_broadcast;

            tokio::select! {
                // Handle incoming commands
                cmd = self.receiver.recv() => {
                    match cmd {
                        Some(BroadcastCommand::Broadcast) => {
                            self.start_broadcast_cycle();
                        }
                        Some(BroadcastCommand::Rebroadcast) => {
                            self.rebroadcast();
                        }
                        Some(BroadcastCommand::AddTransaction(data)) => {
                            self.add_transaction(data.tx_hash, data.envelope);
                        }
                        Some(BroadcastCommand::RemoveTransaction { tx_hash }) => {
                            self.remove_transaction(&tx_hash);
                        }
                        Some(BroadcastCommand::MarkBroadcast { tx_hash }) => {
                            self.broadcast_set.insert(tx_hash);
                        }
                        Some(BroadcastCommand::SetFloodPeriod { period_ms }) => {
                            self.flood_period = Duration::from_millis(period_ms);
                        }
                        Some(BroadcastCommand::Shutdown) | None => {
                            info!("Transaction broadcast manager shutting down");
                            self.state = BroadcastState::Shutdown;
                            break;
                        }
                    }
                }

                // Handle broadcast timer
                _ = crate::herder_utils::sleep_until_or_forever(timeout) => {
                    if self.state == BroadcastState::Waiting {
                        self.broadcast_some();
                    }
                }
            }
        }
    }

    /// Start a broadcast cycle.
    fn start_broadcast_cycle(&mut self) {
        if self.state == BroadcastState::Shutdown {
            return;
        }

        // Schedule the first broadcast
        self.state = BroadcastState::Waiting;
        self.next_broadcast = Some(Instant::now() + self.flood_period);
        debug!(
            "Started broadcast cycle with {}ms period",
            self.flood_period.as_millis()
        );
    }

    /// Force rebroadcast of all transactions.
    fn rebroadcast(&mut self) {
        if self.state == BroadcastState::Shutdown {
            return;
        }

        // Clear the broadcast set to force rebroadcast
        self.broadcast_set.clear();

        // Reset broadcast counts
        for (_, pending_tx) in self.pending.iter_mut() {
            pending_tx.broadcast_count = 0;
            pending_tx.last_broadcast = None;
        }

        info!("Rebroadcasting {} transactions", self.pending.len());

        // Start broadcast cycle
        self.start_broadcast_cycle();
    }

    /// Broadcast some transactions.
    fn broadcast_some(&mut self) {
        if self.state == BroadcastState::Shutdown {
            return;
        }

        let capacity = self.callback.get_flood_capacity();
        let mut broadcast_count = 0;

        // Get transactions sorted by priority from the callback
        let transactions = self.callback.get_transactions_by_priority();

        for (tx_hash, envelope, already_broadcast) in &transactions {
            if broadcast_count >= capacity {
                break;
            }

            // Skip if already broadcast in this cycle
            if *already_broadcast || self.broadcast_set.contains(tx_hash) {
                continue;
            }

            // Broadcast the transaction
            if self.callback.broadcast_transaction(envelope) {
                self.broadcast_set.insert(tx_hash.clone());
                broadcast_count += 1;

                // Update pending state if we're tracking this tx
                if let Some(pending_tx) = self.pending.get_mut(tx_hash) {
                    pending_tx.broadcast_count += 1;
                    pending_tx.last_broadcast = Some(Instant::now());
                }

                trace!(tx_hash = %hex::encode(&tx_hash.0[..8]), "Broadcast transaction");
            }
        }

        debug!(
            "Broadcast {} transactions (capacity: {})",
            broadcast_count, capacity
        );

        // Check if we need to continue broadcasting
        let more_to_broadcast = transactions
            .iter()
            .any(|(h, _, b)| !b && !self.broadcast_set.contains(h));

        if more_to_broadcast && broadcast_count > 0 {
            // Schedule next batch
            self.next_broadcast = Some(Instant::now() + self.flood_period);
        } else {
            // Done broadcasting
            self.state = BroadcastState::Idle;
            self.next_broadcast = None;
        }
    }

    /// Add a transaction to pending.
    fn add_transaction(&mut self, tx_hash: Hash, envelope: TransactionEnvelope) {
        self.pending.insert(
            tx_hash,
            PendingTx {
                _envelope: envelope,
                broadcast_count: 0,
                last_broadcast: None,
            },
        );
    }

    /// Remove a transaction.
    fn remove_transaction(&mut self, tx_hash: &Hash) {
        self.pending.remove(tx_hash);
        self.broadcast_set.remove(tx_hash);
    }
}

/// Statistics about transaction broadcasting.
#[derive(Debug, Clone, Default)]
pub struct TxBroadcastStats {
    /// Number of transactions broadcast.
    pub transactions_broadcast: u64,
    /// Number of broadcast cycles completed.
    pub broadcast_cycles: u64,
    /// Number of rebroadcast requests.
    pub rebroadcast_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::RwLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::time::timeout;

    struct TestCallback {
        broadcast_count: AtomicU64,
        transactions: RwLock<Vec<(Hash, TransactionEnvelope, bool)>>,
    }

    impl TestCallback {
        fn new() -> Self {
            Self {
                broadcast_count: AtomicU64::new(0),
                transactions: RwLock::new(Vec::new()),
            }
        }

        fn add_tx(&self, hash: Hash, envelope: TransactionEnvelope) {
            self.transactions.write().push((hash, envelope, false));
        }
    }

    impl TxBroadcastCallback for TestCallback {
        fn broadcast_transaction(&self, _envelope: &TransactionEnvelope) -> bool {
            self.broadcast_count.fetch_add(1, Ordering::SeqCst);
            true
        }

        fn get_flood_capacity(&self) -> usize {
            10
        }

        fn get_transactions_by_priority(&self) -> Vec<(Hash, TransactionEnvelope, bool)> {
            self.transactions.read().clone()
        }
    }

    fn make_test_envelope() -> TransactionEnvelope {
        use stellar_xdr::curr::*;
        TransactionEnvelope::TxV0(TransactionV0Envelope {
            tx: TransactionV0 {
                source_account_ed25519: Uint256([0u8; 32]),
                fee: 100,
                seq_num: SequenceNumber(1),
                time_bounds: None,
                memo: Memo::None,
                operations: vec![].try_into().unwrap(),
                ext: TransactionV0Ext::V0,
            },
            signatures: vec![].try_into().unwrap(),
        })
    }

    #[tokio::test]
    async fn test_broadcast_cycle() {
        let callback = Arc::new(TestCallback::new());

        // Add some transactions
        for i in 0..5 {
            let hash = Hash([i; 32]);
            callback.add_tx(hash, make_test_envelope());
        }

        let (handle, manager) = TxBroadcastManager::new(callback.clone());

        let manager_task = tokio::spawn(manager.run());

        // Start broadcast cycle
        handle.broadcast().await;

        // Wait for broadcast to complete
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Should have broadcast all 5 transactions
        assert_eq!(callback.broadcast_count.load(Ordering::SeqCst), 5);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }

    #[tokio::test]
    async fn test_rebroadcast() {
        let callback = Arc::new(TestCallback::new());

        // Add some transactions
        for i in 0..3 {
            let hash = Hash([i; 32]);
            callback.add_tx(hash, make_test_envelope());
        }

        let (handle, manager) = TxBroadcastManager::new(callback.clone());

        let manager_task = tokio::spawn(manager.run());

        // First broadcast
        handle.broadcast().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(callback.broadcast_count.load(Ordering::SeqCst), 3);

        // Rebroadcast
        handle.rebroadcast().await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Should have broadcast 6 total (3 + 3)
        assert_eq!(callback.broadcast_count.load(Ordering::SeqCst), 6);

        handle.shutdown().await;
        let _ = timeout(Duration::from_millis(100), manager_task).await;
    }
}
