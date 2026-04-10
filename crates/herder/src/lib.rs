//! SCP coordination and ledger close orchestration for rs-stellar-core.
//!
//! The Herder is the central coordinator that bridges the overlay network and the
//! ledger manager through SCP (Stellar Consensus Protocol). It manages the flow
//! from receiving transactions and SCP messages, through consensus, to triggering
//! ledger close.
//!
//! # Crate Overview
//!
//! This crate provides:
//!
//! - [`Herder`]: The main coordinator for consensus and ledger close
//! - [`TransactionQueue`]: Pending transaction management with surge pricing
//! - [`TransactionSet`]: A set of transactions for a ledger
//! - [`HerderState`]: The state machine for the Herder
//! - [`HerderConfig`]: Configuration options for the Herder
//! - [`EnvelopeState`]: Result of receiving an SCP envelope
//! - [`FetchingEnvelopes`]: Envelope dependency fetching (TxSet/QuorumSet)
//! - [`ScpDriver`]: Bridge between SCP consensus and application logic
//! - [`QuorumTracker`]: Transitive quorum set membership tracking
//! - [`LedgerCloseData`]: Complete ledger close information from consensus
//! - [`ScpPersistenceManager`]: SCP state persistence for crash recovery
//!
//! # Architecture
//!
//! ```text
//! +------------------+     +------------------+
//! |  Overlay Network | --> |      Herder      |
//! +------------------+     +------------------+
//!                                |
//!      +------------+------------+------------+
//!      |            |            |            |
//!      v            v            v            v
//! +----------+ +----------+ +---------+ +----------+
//! |  TxQueue | | Fetching | | Pending | | ScpDriver|
//! +----------+ | Envelopes| | Envlps  | +----------+
//!      |       +----------+ +---------+      |
//!      v                                     v
//! +----------+                       +----------------+
//! |  Surge   |                       | SCP Consensus  |
//! | Pricing  |                       +----------------+
//! +----------+
//! ```
//!
//! # State Machine
//!
//! The Herder progresses through states:
//!
//! 1. **Booting**: Initial state, not connected to network
//! 2. **Syncing**: Catching up with the network via history archives
//! 3. **Tracking**: Synchronized and following consensus in real-time
//!
//! # Operating Modes
//!
//! - **Observer mode**: Tracks consensus by observing EXTERNALIZE messages from
//!   validators in the quorum. Does not vote or propose values.
//! - **Validator mode**: Actively participates in consensus by proposing transaction
//!   sets and voting. Requires a secret key and quorum set configuration.
//!
//! # Example
//!
//! ```ignore
//! use henyey_herder::{Herder, HerderConfig, HerderState};
//!
//! // Create a non-validator herder
//! let config = HerderConfig::default();
//! let herder = Herder::new(config);
//!
//! // Start syncing (when catchup begins)
//! herder.start_syncing();
//! assert_eq!(herder.state(), HerderState::Syncing);
//!
//! // After catchup completes
//! herder.bootstrap(ledger_seq);
//! assert_eq!(herder.state(), HerderState::Tracking);
//!
//! // Process incoming SCP envelopes
//! let state = herder.receive_scp_envelope(envelope);
//!
//! // Process incoming transactions
//! let result = herder.receive_transaction(tx);
//! ```
//!
//! # Modules
//!
//! - `error`: Error types for Herder operations
//! - `herder`: Main Herder implementation and configuration
//! - `scp_driver`: SCP integration callbacks (value validation, signing)
//! - `tx_queue`: Transaction queue and set building
//! - `tx_queue_limiter`: Resource-aware queue limiting with eviction
//! - `surge_pricing`: Lane configuration and priority queues
//! - `pending`: Pending SCP envelope management (future slots)
//! - `fetching_envelopes`: Envelopes waiting for TxSet/QuorumSet from peers
//! - `quorum_tracker`: Quorum participation and security tracking
//! - `persistence`: SCP state persistence for crash recovery (SQLite)
//! - `upgrades`: Ledger upgrade scheduling and validation
//! - `ledger_close_data`: Ledger close data for consensus output
//! - `herder_utils`: Utility functions (value extraction, node ID formatting)
//! - `timer_manager`: SCP nomination/ballot timeout scheduling (tokio)
//! - `sync_recovery`: Out-of-sync detection and recovery
//! - `tx_broadcast`: Periodic transaction flooding to peers
//! - `drift_tracker`: Close time drift monitoring
//! - `dead_node_tracker`: Missing/dead validator detection
//! - `flow_control`: Flow control constants (transaction size limits)
//! - `json_api`: JSON structures for admin/diagnostic endpoints
//! - `state`: Herder state machine definition

pub mod dead_node_tracker;
pub mod drift_tracker;
mod error;
pub mod fetching_envelopes;
pub mod flow_control;
mod herder;
mod herder_utils;
pub mod json_api;
mod ledger_close_data;
pub mod parallel_tx_set_builder;
mod pending;
mod persistence;
mod quorum_set_tracker;
mod quorum_tracker;
mod scp_driver;
mod state;
mod surge_pricing;
pub mod sync_recovery;
pub mod timer_manager;
pub mod tx_broadcast;
mod tx_queue;
mod tx_queue_limiter;
mod tx_set_tracker;
pub mod tx_set_utils;
pub mod upgrades;

// Re-export main types
pub use error::HerderError;
pub use fetching_envelopes::{FetchingConfig, FetchingEnvelopes, FetchingStats, RecvResult};
pub use herder::{EnvelopeState, Herder, HerderConfig, HerderStats, LedgerCloseInfo};
pub use pending::{PendingConfig, PendingEnvelopes, PendingResult, PendingStats};
pub use quorum_tracker::{QuorumTracker, SlotQuorumTracker};
pub use scp_driver::{
    CachedTxSet, ExternalizedSlot, HerderScpCallback, PendingTxSet, ScpDriver, ScpDriverCacheSizes,
    ScpDriverConfig, ValueValidation,
};
pub use state::HerderState;
pub use tx_queue::{
    QueuedTransaction, SorobanTxLimits, TransactionQueue, TransactionSet, TxQueueConfig,
    TxQueueResult, TxQueueStats,
};

// Persistence
pub use persistence::{
    get_quorum_set_hash, get_tx_set_hashes, Database, InMemoryScpPersistence, PersistedSlotState,
    RestoredScpState, ScpPersistenceManager, ScpStatePersistence, SqliteScpPersistence,
};

// HerderUtils
pub use herder_utils::{
    get_stellar_values, get_tx_set_hashes_from_envelope, to_short_string, to_short_strkey,
};

// LedgerCloseData
pub use ledger_close_data::{stellar_value_to_string, LedgerCloseData, LedgerCloseDataError};

// TxSetUtils
pub use tx_set_utils::{
    get_invalid_tx_list, get_invalid_tx_list_with_fee_map, trim_invalid, trim_invalid_two_phase,
    CloseTimeBounds, TxSetValidationContext,
};

// Fee balance provider trait (used by tx_set_utils for affordability checks)
pub use tx_queue::FeeBalanceProvider;

// Parallel tx set builder
pub use parallel_tx_set_builder::build_two_phase_tx_set;

// TxQueueLimiter and surge pricing
pub use surge_pricing::VisitTxResult;
pub use tx_queue_limiter::TxQueueLimiter;

// Timer management
pub use timer_manager::{
    TimerCallback, TimerCommand, TimerManager, TimerManagerHandle, TimerManagerWithStats,
    TimerStats, TimerType,
};

// Sync recovery
pub use sync_recovery::{
    SyncRecoveryCallback, SyncRecoveryCommand, SyncRecoveryHandle, SyncRecoveryManager,
    SyncRecoveryStats, SyncRecoveryStatsTracker, CONSENSUS_STUCK_TIMEOUT, LEDGER_VALIDITY_BRACKET,
    OUT_OF_SYNC_RECOVERY_INTERVAL,
};

// Transaction broadcast
pub use tx_broadcast::{
    BroadcastCommand, TxBroadcastCallback, TxBroadcastHandle, TxBroadcastManager, TxBroadcastStats,
    DEFAULT_FLOOD_PERIOD_MS,
};

// Dead node detection
pub use dead_node_tracker::{DeadNodeTracker, CHECK_FOR_DEAD_NODES_MINUTES};

/// Result type for Herder operations.
pub type Result<T> = std::result::Result<T, HerderError>;

/// A pending transaction waiting to be included in a ledger.
///
/// Tracks metadata about a transaction received from the network, including
/// when it was first seen and how many times it has been broadcast.
#[derive(Debug)]
pub struct PendingTransaction {
    /// The transaction envelope containing the transaction and signatures.
    pub envelope: stellar_xdr::curr::TransactionEnvelope,
    /// When this transaction was first received from the network.
    pub received_at: std::time::Instant,
    /// Number of times this transaction has been seen/broadcast.
    pub broadcast_count: u32,
}

/// A value that has been externalized by SCP.
///
/// Represents the consensus output for a specific ledger slot, containing
/// the information needed to close the ledger.
#[derive(Debug, Clone)]
pub struct ExternalizedValue {
    /// The ledger sequence number this value is for.
    pub ledger_seq: u32,
    /// Hash of the transaction set agreed upon by consensus.
    pub tx_set_hash: henyey_common::Hash256,
    /// The ledger close time agreed upon by consensus (Unix timestamp).
    pub close_time: u64,
}

/// Trait for Herder callbacks.
///
/// Implementers receive notifications from the Herder when ledgers should be
/// closed or messages should be broadcast. This allows the Herder to remain
/// decoupled from the specific ledger and overlay implementations.
#[async_trait::async_trait]
pub trait HerderCallback: Send + Sync {
    /// Called when consensus has been reached and a ledger should be closed.
    ///
    /// # Arguments
    ///
    /// * `ledger_seq` - The sequence number of the ledger to close
    /// * `tx_set` - The agreed-upon transaction set
    /// * `close_time` - The ledger close time
    /// * `upgrades` - Any protocol upgrades to apply
    /// * `stellar_value_ext` - The StellarValue extension (Basic or Signed) from consensus
    ///
    /// # Returns
    ///
    /// The hash of the new ledger header after closing.
    async fn close_ledger(
        &self,
        ledger_seq: u32,
        tx_set: TransactionSet,
        close_time: u64,
        upgrades: Vec<stellar_xdr::curr::UpgradeType>,
        stellar_value_ext: stellar_xdr::curr::StellarValueExt,
    ) -> Result<henyey_common::Hash256>;

    /// Called when an SCP message should be broadcast to the network.
    ///
    /// The implementer should relay this envelope to connected peers.
    async fn broadcast_scp_message(&self, envelope: stellar_xdr::curr::ScpEnvelope);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = HerderConfig::default();
        assert_eq!(config.max_pending_transactions, 1000);
        assert!(!config.is_validator);
        assert_eq!(config.ledger_close_time, 5);
    }

    #[test]
    fn test_herder_creation() {
        let config = HerderConfig::default();
        let herder = Herder::new(config);
        assert_eq!(herder.state(), HerderState::Booting);
    }

    #[test]
    fn test_state_transitions() {
        let config = HerderConfig::default();
        let herder = Herder::new(config);

        assert_eq!(herder.state(), HerderState::Booting);

        herder.start_syncing();
        assert_eq!(herder.state(), HerderState::Syncing);

        herder.bootstrap(100);
        assert_eq!(herder.state(), HerderState::Tracking);
        assert_eq!(herder.tracking_slot(), 101);
    }
}
