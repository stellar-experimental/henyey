//! Transaction queue management.
//!
//! The transaction queue holds pending transactions waiting to be included
//! in a ledger. Transactions are ordered by fee (highest first) to maximize
//! network efficiency and incentivize appropriate fee bidding.
//!
//! # Overview
//!
//! The [`TransactionQueue`] is the central component for transaction mempool
//! management. It handles:
//!
//! - **Transaction validation**: Structural, time bounds, and signature checks
//! - **Fee-based ordering**: Higher-fee transactions are prioritized
//! - **Sequence number handling**: Maintains contiguous sequences per account
//! - **Lane-based limits**: Separate limits for classic, DEX, and Soroban transactions
//! - **Eviction**: Lower-fee transactions are evicted when limits are exceeded
//! - **Per-account limits**: One transaction per account (sequence-number-source)
//! - **Fee balance validation**: Validates fee-source has sufficient balance
//!
//! # Transaction Set Building
//!
//! When building a transaction set for consensus, the queue:
//!
//! 1. Groups transactions by source account
//! 2. Ensures contiguous sequence numbers (gaps break the chain)
//! 3. Separates classic and Soroban transactions into different phases
//! 4. Applies surge pricing when demand exceeds capacity
//! 5. Produces a [`GeneralizedTransactionSet`] (protocol 20+) or legacy format
//!
//! # Sequence Number Rules
//!
//! For a given account, only transactions with contiguous sequence numbers
//! can be included in the same ledger. Additionally, once a Soroban transaction
//! appears in the sequence, subsequent classic transactions are excluded
//! (Soroban and classic transactions execute in different phases).
//!
//! # Per-Account Limits
//!
//! The queue enforces a one-transaction-per-account limit (based on the
//! sequence-number-source). Fee-bump transactions can replace an existing
//! transaction with the same sequence number if the new fee is at least
//! 10x the existing fee rate. Transactions that are not included in a ledger
//! for too many consecutive ledgers (pending_depth) are automatically banned.

use parking_lot::RwLock;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use henyey_common::{
    any_greater, xdr_to_bytes, Hash256, NetworkId, Resource, ResourceType, NUM_SOROBAN_TX_RESOURCES,
};
use henyey_crypto::Sha256Hasher;
use stellar_xdr::WriteXdr;
use stellar_xdr::{
    AccountEntry, AccountId, DecoratedSignature, FeeBumpTransactionInnerTx,
    GeneralizedTransactionSet, Limits, OperationType, Preconditions, SignerKey,
    TransactionEnvelope, TransactionPhase, TxSetComponent,
};

use crate::error::HerderError;
use crate::surge_pricing::{
    DexLimitingLaneConfig, EvictionExclusion, OpsOnlyLaneConfig, QueueEntry,
    SorobanGenericLaneConfig, SurgePricingLaneConfig, SurgePricingPriorityQueue, VisitTxResult,
    GENERIC_LANE,
};
use crate::Result;
use henyey_tx::envelope_sequence_number;
use henyey_tx::FeeRate;
use rand::Rng;

pub mod arb_flood_damping;
pub(crate) mod flood_queue;
mod selection;
mod tx_set;

pub(crate) use selection::{BuildContext, NominationBuildContext};
pub use tx_set::*;

/// Result of attempting to add a transaction to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxQueueResult {
    /// Transaction was added successfully.
    Added,
    /// Transaction is a duplicate.
    Duplicate,
    /// Queue is full.
    QueueFull,
    /// Transaction fee is too low.
    FeeTooLow,
    /// Transaction is invalid. Contains the specific error code when available.
    Invalid(Option<henyey_tx::TxResultCode>),
    /// Transaction is banned.
    Banned,
    /// Transaction contains a filtered operation type.
    Filtered,
    /// Account already has a pending transaction. Try again later or use fee-bump.
    TryAgainLater,
}

/// Result of the shift() operation after ledger close.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShiftResult {
    /// Number of transactions that were unbanned (reached end of ban period).
    pub unbanned_count: usize,
    /// Number of transactions that were auto-banned due to age (pending too long).
    pub evicted_due_to_age: usize,
    /// Pending transactions that reached age 2 this shift (sustained-load
    /// wedge fix): the caller pushes their full bodies to all peers, so a tx
    /// whose advert/demand/response was lost anywhere recovers within one
    /// ledger instead of wedging until its account's next submission collides
    /// (fatal for PayPregenerated load runs, #3638). Rare by construction —
    /// only txs that missed 2 consecutive ledgers.
    pub reflooded_txs: Vec<TransactionEnvelope>,
}

const MAX_TX_SET_ALLOWANCE_BYTES: u32 = 10 * 1024 * 1024;
const MAX_CLASSIC_BYTE_ALLOWANCE: u32 = MAX_TX_SET_ALLOWANCE_BYTES / 2;
const MAX_SOROBAN_BYTE_ALLOWANCE: u32 = MAX_TX_SET_ALLOWANCE_BYTES / 2;

/// Default maximum number of transactions in the queue.
const DEFAULT_MAX_QUEUE_SIZE: usize = 1000;
/// Default maximum age (seconds) before a pending transaction is evicted (5 minutes).
const DEFAULT_MAX_AGE_SECS: u64 = 300;
/// Default minimum fee per operation (100 stroops = 0.00001 XLM).
const DEFAULT_MIN_FEE_PER_OP: u32 = 100;
/// Multiplier for expected close time in upper bound offset calculation.
/// Parity: stellar-core `EXPECTED_CLOSE_TIME_MULT` in TransactionUtils.h.
const EXPECTED_CLOSE_TIME_MULT: u64 = 2;

/// Multiplier applied to the per-ledger resource limits to derive the classic
/// transaction queue's admission capacity:
/// `maxQueueResources = maxLedgerResources × TRANSACTION_QUEUE_SIZE_MULTIPLIER`.
///
/// Parity: stellar-core `Config::TRANSACTION_QUEUE_SIZE_MULTIPLIER`
/// (`src/main/Config.cpp:205`, default `2`) and HERDER_SPEC §12.6. This is
/// distinct from the pending depth (`TRANSACTION_QUEUE_TIMEOUT_LEDGERS = 4`); the multiplier
/// scales the resource-based pool capacity, not the number of ledgers a tx may
/// remain pending.
pub const TRANSACTION_QUEUE_SIZE_MULTIPLIER: u32 = 2;

/// Multiplier applied to the per-ledger resource limits to derive the Soroban
/// transaction queue's admission capacity:
/// `maxQueueResources = maxLedgerResources × SOROBAN_TRANSACTION_QUEUE_SIZE_MULTIPLIER`.
///
/// Parity: stellar-core `Config::SOROBAN_TRANSACTION_QUEUE_SIZE_MULTIPLIER`
/// (`src/main/Config.cpp:206`, default `2`) and HERDER_SPEC §12.6.
pub const SOROBAN_TRANSACTION_QUEUE_SIZE_MULTIPLIER: u32 = 2;

/// Trait for providing ledger account balance information.
///
/// This trait is used for fee balance validation during transaction queue
/// operations. Implementations should provide the available balance for
/// an account that can be used to pay transaction fees.
pub trait FeeBalanceProvider: Send + Sync {
    /// Get the available balance for an account that can be used for fees.
    ///
    /// Returns `Ok(None)` if the account doesn't exist, `Ok(Some(balance))`
    /// on success, or `Err(...)` on I/O or snapshot failure.
    fn get_available_balance(&self, account_id: &AccountId) -> henyey_ledger::Result<Option<i64>>;
}

/// Trait for providing account data to tx-set validation.
///
/// This mirrors the `FeeBalanceProvider` pattern. Implementations should look up
/// accounts from a ledger snapshot so that tx-set validation can verify sequence
/// numbers, signatures, and account existence — matching stellar-core's
/// `getInvalidTxListWithErrors` which calls `tx->checkValid(app, ls, ...)`.
pub trait AccountProvider: Send + Sync {
    /// Load an account entry by account ID.
    ///
    /// Returns `Ok(None)` if the account does not exist, `Ok(Some(entry))`
    /// on success, or `Err(...)` on I/O or snapshot failure.
    fn load_account(&self, account_id: &AccountId) -> henyey_ledger::Result<Option<AccountEntry>>;
}

/// Single-snapshot provider for batch tx-set validation.
///
/// Wraps one [`henyey_ledger::SnapshotHandle`] and impls both [`AccountProvider`]
/// and [`FeeBalanceProvider`], so the same frozen snapshot serves all lookups
/// during a nomination or post-close validation pass.
///
/// # Parity
///
/// Mirrors stellar-core's single `LedgerSnapshot ls(app)` per
/// `getInvalidTxListWithErrors` call (`TxSetUtils.cpp:167`).
///
/// # When to use
///
/// * **Batch paths** (N txs → 1 snapshot): use this type.
/// * **Admission paths** (1 tx → 1 snapshot per call): keep the per-call
///   providers on the queue — no amplification, no benefit.
pub struct SnapshotProviders {
    snapshot: henyey_ledger::SnapshotHandle,
}

impl SnapshotProviders {
    /// Build providers from an existing snapshot handle.
    pub fn new(snapshot: henyey_ledger::SnapshotHandle) -> Self {
        Self { snapshot }
    }

    /// Access the underlying snapshot (e.g., for reading header/base_reserve).
    pub fn snapshot(&self) -> &henyey_ledger::SnapshotHandle {
        &self.snapshot
    }
}

impl AccountProvider for SnapshotProviders {
    fn load_account(&self, account_id: &AccountId) -> henyey_ledger::Result<Option<AccountEntry>> {
        self.snapshot.get_account(account_id)
    }
}

impl FeeBalanceProvider for SnapshotProviders {
    fn get_available_balance(&self, account_id: &AccountId) -> henyey_ledger::Result<Option<i64>> {
        let Some(acc) = self.snapshot.get_account(account_id)? else {
            return Ok(None);
        };
        let base_reserve = self.snapshot.header().base_reserve;
        Ok(Some(henyey_ledger::reserves::available_to_send(
            &acc,
            base_reserve,
        )))
    }
}

/// Configuration for the transaction queue.
#[derive(Debug, Clone)]
pub struct TxQueueConfig {
    /// Maximum number of transactions in the queue.
    pub max_size: usize,
    /// Maximum age of a transaction before it's evicted (in seconds).
    pub max_age_secs: u64,
    /// Minimum fee per operation in stroops.
    pub min_fee_per_op: u32,
    /// Whether to validate signatures before queueing.
    pub validate_signatures: bool,
    /// Whether to validate time/ledger bounds before queueing.
    pub validate_bounds: bool,
    /// Network ID for signature validation.
    pub network_id: NetworkId,
    /// Optional limit for DEX operation counts within a tx set.
    pub max_dex_ops: Option<u32>,
    /// Optional classic tx byte allowance for tx set selection.
    pub max_classic_bytes: Option<u32>,
    /// Optional byte allowance for DEX lane tx set selection.
    pub max_dex_bytes: Option<u32>,
    /// Optional Soroban resource limit for tx set selection.
    pub max_soroban_resources: Option<Resource>,
    /// Optional Soroban tx byte allowance for tx set selection.
    pub max_soroban_bytes: Option<u32>,
    /// Optional limit for DEX operation counts within the queue.
    pub max_queue_dex_ops: Option<u32>,
    /// Optional Soroban resource limit for queue admission.
    pub max_queue_soroban_resources: Option<Resource>,
    /// Optional total op limit for queue admission.
    pub max_queue_ops: Option<u32>,
    /// Optional classic tx byte allowance for queue admission.
    pub max_queue_classic_bytes: Option<u32>,
    /// Operation types to filter out (transactions containing these will be rejected).
    ///
    /// This allows nodes to exclude transactions with specific operation types
    /// from their mempool. This is configured via
    /// `EXCLUDE_TRANSACTIONS_CONTAINING_OPERATION_TYPE` in stellar-core.
    pub filtered_operation_types: HashSet<OperationType>,
    /// Maximum ledger-wide Soroban instructions (from ContractComputeV0).
    /// Used for parallel phase building. Default 0 disables parallel building.
    pub ledger_max_instructions: i64,
    /// Maximum dependent TX clusters per stage (from ContractParallelComputeV0).
    /// Used for parallel phase building. Default 0 disables parallel building.
    pub ledger_max_dependent_tx_clusters: u32,
    /// Minimum number of stages to try when building the parallel Soroban phase.
    pub soroban_phase_min_stage_count: u32,
    /// Maximum number of stages to try when building the parallel Soroban phase.
    pub soroban_phase_max_stage_count: u32,
    /// Expected ledger close time in seconds (used for upper bound close time offset).
    /// Matches stellar-core's `EXPECTED_LEDGER_CLOSE_TIME` config.
    pub expected_ledger_close_secs: u64,
    /// Arbitrage flood damping: number of unconditional broadcasts per asset
    /// pair per ledger. Set to `-1` to disable damping. Default `5`.
    /// Matches stellar-core `FLOOD_ARB_TX_BASE_ALLOWANCE`.
    pub flood_arb_tx_base_allowance: i32,
    /// Arbitrage flood damping: probability parameter for the geometric
    /// distribution used beyond the base allowance. Must be in `(0.0, 1.0]`.
    /// Default `0.8`. Matches stellar-core `FLOOD_ARB_TX_DAMPING_FACTOR`.
    pub flood_arb_tx_damping_factor: f64,
}

impl Default for TxQueueConfig {
    fn default() -> Self {
        Self {
            max_size: DEFAULT_MAX_QUEUE_SIZE,
            max_age_secs: DEFAULT_MAX_AGE_SECS,
            min_fee_per_op: DEFAULT_MIN_FEE_PER_OP,
            validate_signatures: true,
            validate_bounds: true,
            network_id: NetworkId::testnet(),
            max_dex_ops: None,
            max_classic_bytes: Some(MAX_CLASSIC_BYTE_ALLOWANCE),
            max_dex_bytes: None,
            max_soroban_resources: None,
            max_soroban_bytes: Some(MAX_SOROBAN_BYTE_ALLOWANCE),
            max_queue_dex_ops: None,
            max_queue_soroban_resources: None,
            max_queue_ops: None,
            max_queue_classic_bytes: None,
            filtered_operation_types: HashSet::new(),
            ledger_max_instructions: 0,
            ledger_max_dependent_tx_clusters: 0,
            soroban_phase_min_stage_count: 1,
            soroban_phase_max_stage_count: 4,
            expected_ledger_close_secs: 5,
            flood_arb_tx_base_allowance: 5,
            flood_arb_tx_damping_factor: 0.8,
        }
    }
}

/// Default base reserve in stroops (0.5 XLM).
const DEFAULT_BASE_RESERVE: u32 = 5_000_000;

/// Validation context for transaction queue.
#[derive(Debug, Clone)]
pub struct ValidationContext {
    /// Current ledger sequence.
    pub ledger_seq: u32,
    /// Current close time (Unix timestamp).
    pub close_time: u64,
    /// Protocol version.
    pub protocol_version: u32,
    /// Current ledger base fee (stroops per op).
    pub base_fee: u32,
    /// Base reserve per ledger entry (stroops).
    pub base_reserve: u32,
    /// Ledger header flags (e.g. LP disable flags). 0 if pre-v1 extension.
    pub ledger_flags: u32,
    /// Expected ledger close time.
    /// Updated each ledger close from the dynamic network config.
    pub expected_close_time: Duration,
    /// Soroban per-transaction resource limits (if available).
    pub soroban_limits: Option<SorobanTxLimits>,
    /// Per-TX Soroban resource limits for check_valid_pre_seq_num validation.
    pub soroban_resource_limits: Option<henyey_tx::SorobanResourceLimits>,
    /// CAP-77: Frozen ledger key configuration (from Soroban network config).
    /// Pre-V26 this is None (no frozen keys).
    pub frozen_key_config: Option<henyey_tx::frozen_keys::FrozenKeyConfig>,
}

/// Per-transaction Soroban resource limits from network config.
///
/// Parity: stellar-core `SorobanNetworkConfig` tx-level limits.
#[derive(Debug, Clone)]
pub struct SorobanTxLimits {
    /// Maximum instructions per transaction.
    pub tx_max_instructions: u64,
    /// Maximum disk read bytes per transaction.
    pub tx_max_read_bytes: u64,
    /// Maximum write bytes per transaction.
    pub tx_max_write_bytes: u64,
    /// Maximum read ledger entries per transaction.
    pub tx_max_read_ledger_entries: u64,
    /// Maximum write ledger entries per transaction.
    pub tx_max_write_ledger_entries: u64,
    /// Maximum transaction size in bytes.
    pub tx_max_size_bytes: u64,
}

impl Default for ValidationContext {
    fn default() -> Self {
        Self {
            ledger_seq: 0,
            close_time: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            protocol_version: 21,
            base_fee: 100,
            base_reserve: DEFAULT_BASE_RESERVE,
            ledger_flags: 0,
            expected_close_time: Duration::from_secs(5),
            soroban_limits: None,
            soroban_resource_limits: None,
            frozen_key_config: None,
        }
    }
}

/// A transaction in the queue with metadata.
///
/// All fields are private to enforce invariants established by `new()`:
/// - `hash == Hash256::hash_xdr(&envelope)`
/// - `fee_rate` correctly reflects inclusion fee and op count
/// - `fee_per_op` is correctly derived from `fee_rate`
/// - `total_fee` matches the envelope's declared fee
/// - `is_dex` correctly reflects envelope operations
#[derive(Debug, Clone)]
pub struct QueuedTransaction {
    /// The transaction envelope (shared via Arc to avoid deep-cloning).
    envelope: Arc<TransactionEnvelope>,
    /// Hash of the transaction.
    hash: Hash256,
    /// When this transaction was received.
    received_at: Instant,
    /// Declared fee per operation used for queue-admission minimum fee checks.
    fee_per_op: u64,
    /// Fee rate (bundles inclusion_fee + op_count) for surge pricing and replacement decisions.
    fee_rate: FeeRate,
    /// Declared full fee.
    total_fee: u64,
    /// Whether this transaction contains DEX operations (cached at admission).
    is_dex: bool,
}

impl QueuedTransaction {
    /// Create a new queued transaction.
    pub fn new(envelope: TransactionEnvelope) -> Result<Self> {
        let hash = Hash256::hash_xdr(&envelope);

        let (total_fee, fee_rate) = Self::extract_fees_and_ops(&envelope)?;
        // `fee_per_op` is the inclusion-fee per op, used by
        // `ordered_hashes_by_fee` for ranking.  Using inclusion fee (not
        // total fee) matches the rest of the queue's fee-rate metrics
        // and stellar-core's `computePerOpFee`, which calls
        // `tx.getInclusionFee()` (`stellar-core/src/herder/TxSetFrame.cpp:213-223`).
        // For classic (non-Soroban) txs, `inclusion_fee == total_fee`,
        // so this is a no-op in the classic path.
        //
        // `extract_fees_and_ops` already rejects negative inclusion fees;
        // `inclusion_fee >= 0` is invariant here.
        let fee_per_op = if fee_rate.op_count() > 0 {
            debug_assert!(
                fee_rate.inclusion_fee().as_i64() >= 0,
                "inclusion_fee must be non-negative here"
            );
            fee_rate.inclusion_fee().as_i64() as u64 / fee_rate.op_count() as u64
        } else {
            0
        };

        Ok(Self {
            is_dex: henyey_tx::envelope_utils::has_dex_operations_envelope(&envelope),
            envelope: Arc::new(envelope),
            hash,
            received_at: Instant::now(),
            fee_per_op,
            fee_rate,
            total_fee,
        })
    }

    /// Extract full fee and fee rate from the envelope.
    fn extract_fees_and_ops(envelope: &TransactionEnvelope) -> Result<(u64, FeeRate)> {
        let fee = crate::tx_set_utils::envelope_fee(envelope).as_i64();
        if fee < 0 {
            return Err(HerderError::Internal(format!(
                "Negative declared fee for transaction: {}",
                fee
            )));
        }
        let inclusion_fee = crate::tx_set_utils::envelope_inclusion_fee(envelope);
        if inclusion_fee.as_i64() < 0 {
            return Err(HerderError::Internal(format!(
                "Negative inclusion fee for transaction: {}",
                inclusion_fee.as_i64()
            )));
        }
        let ops = crate::tx_set_utils::envelope_num_ops(envelope) as u32;
        Ok((fee as u64, FeeRate::new(inclusion_fee, ops)))
    }

    /// Get the operation count.
    #[inline]
    pub fn op_count(&self) -> u32 {
        self.fee_rate.op_count()
    }

    /// Get the inclusion fee as i64.
    #[inline]
    pub fn inclusion_fee_i64(&self) -> i64 {
        self.fee_rate.inclusion_fee().as_i64()
    }

    fn sequence_number(&self) -> i64 {
        envelope_sequence_number(&self.envelope)
    }

    pub(crate) fn account_key(&self) -> Vec<u8> {
        account_key(&self.envelope)
    }

    /// Check if this transaction has expired.
    pub fn is_expired(&self, max_age_secs: u64) -> bool {
        self.received_at.elapsed().as_secs() > max_age_secs
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn is_better_than(&self, other: &QueuedTransaction) -> bool {
        better_fee_ratio(self, other)
    }

    /// Compare this transaction's fee rate against a FeeEntry (from the index).
    fn is_better_than_entry(&self, entry: &FeeEntry) -> bool {
        match self.fee_rate.cmp_rate(&entry.fee_rate) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => self.hash.0 < entry.hash.0,
        }
    }

    /// Get the transaction envelope.
    #[inline]
    pub fn envelope(&self) -> &TransactionEnvelope {
        &self.envelope
    }

    /// Get the Arc-wrapped envelope (for cloning the Arc).
    #[inline]
    pub fn arc_envelope(&self) -> &Arc<TransactionEnvelope> {
        &self.envelope
    }

    /// Get the transaction hash.
    #[inline]
    pub fn hash(&self) -> Hash256 {
        self.hash
    }

    /// Get the time this transaction was received.
    #[inline]
    pub fn received_at(&self) -> Instant {
        self.received_at
    }

    /// Get the declared fee per operation.
    #[inline]
    pub fn fee_per_op(&self) -> u64 {
        self.fee_per_op
    }

    /// Get the fee rate.
    #[inline]
    pub fn fee_rate(&self) -> &FeeRate {
        &self.fee_rate
    }

    /// Get the declared full fee.
    #[inline]
    pub fn total_fee(&self) -> u64 {
        self.total_fee
    }

    /// Whether this transaction contains DEX operations.
    #[inline]
    pub fn is_dex(&self) -> bool {
        self.is_dex
    }

    /// Consume self and return the inner envelope, unwrapping the Arc if possible.
    #[inline]
    pub fn into_envelope(self) -> TransactionEnvelope {
        Arc::unwrap_or_clone(self.envelope)
    }

    /// Set the received_at timestamp (test-only).
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn set_received_at(&mut self, t: Instant) {
        self.received_at = t;
    }

    /// Construct a QueuedTransaction with explicit field values (test-only).
    ///
    /// Tests use synthetic hashes for deterministic ordering; they cannot use
    /// `new()` which computes the real hash from the envelope.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        envelope: Arc<TransactionEnvelope>,
        hash: Hash256,
        fee_rate: FeeRate,
        fee_per_op: u64,
        total_fee: u64,
        is_dex: bool,
    ) -> Self {
        Self {
            envelope,
            hash,
            received_at: Instant::now(),
            fee_per_op,
            fee_rate,
            total_fee,
            is_dex,
        }
    }
}

/// Per-account state in the transaction queue.
///
/// Parity: An AccountID is tracked in mAccountStates if and only if:
/// - total_fees > 0 (account is fee-source for at least one tx), OR
/// - transaction.is_some() (account is seq-number-source for a queued tx)
///
/// The fee-source and sequence-number-source can be different accounts
/// (e.g., in fee-bump transactions where another account pays the fee).
#[derive(Debug, Clone, Default)]
pub struct AccountState {
    /// Sum of full fees for all transactions where this account is the fee-source.
    /// This tracks the total fees this account is liable for across all queued
    /// transactions, which may include transactions where the sequence-number-source
    /// is a different account.
    pub total_fees: i64,
    /// Number of ledgers that have closed since the last ledger in which a transaction
    /// from this sequence-number-source was included. Always 0 if transaction is None.
    /// Used for auto-ban: when age reaches pending_depth, the transaction is banned.
    pub age: u32,
    /// The single pending transaction for which this account is the sequence-number-source.
    /// stellar-core enforces one transaction per account (non-fee-bump) in the queue.
    pub transaction: Option<QueuedTransaction>,
    /// Number of queued Soroban transactions for which this account is the fee-source.
    /// Parity: stellar-core's per-queue `mAccountStates` implicitly tracks fee-source
    /// involvement by type (each queue only contains one tx type). In henyey's
    /// single-queue model we need explicit type counts to replicate the cross-queue
    /// source-account guard for fee-source-only entries.
    pub soroban_fee_tx_count: u32,
    /// Number of queued classic transactions for which this account is the fee-source.
    pub classic_fee_tx_count: u32,
}

impl AccountState {
    /// Check if this account state can be removed (no transaction and no fees tracked).
    pub fn is_empty(&self) -> bool {
        self.transaction.is_none()
            && self.total_fees == 0
            && self.soroban_fee_tx_count == 0
            && self.classic_fee_tx_count == 0
    }
}

/// Fee multiplier required for replace-by-fee with fee-bump transactions.
/// A fee-bump must have a fee at least FEE_MULTIPLIER times the existing fee rate.
const FEE_MULTIPLIER: u64 = 10;

/// Default pending depth (number of ledgers before auto-ban).
/// Spec: HERDER_SPEC §17 — TRANSACTION_QUEUE_TIMEOUT_LEDGERS = 4.
/// Parity: stellar-core `HerderImpl.cpp:65` —
/// `constexpr uint32 const TRANSACTION_QUEUE_TIMEOUT_LEDGERS = 4;`.
const TRANSACTION_QUEUE_TIMEOUT_LEDGERS: u32 = 4;

pub(super) fn envelope_fee_per_op(envelope: &TransactionEnvelope) -> Option<(u64, FeeRate)> {
    QueuedTransaction::extract_fees_and_ops(envelope)
        .ok()
        .map(|(_, fee_rate)| {
            let per_op = if fee_rate.op_count() > 0 {
                fee_rate.inclusion_fee().as_i64() as u64 / fee_rate.op_count() as u64
            } else {
                0
            };
            (per_op, fee_rate)
        })
}

fn better_fee_ratio(new_tx: &QueuedTransaction, old_tx: &QueuedTransaction) -> bool {
    match new_tx.fee_rate.cmp_rate(&old_tx.fee_rate) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => new_tx.hash.0 < old_tx.hash.0,
    }
}

fn compute_better_fee(evicted: &FeeRate, tx_ops: u32) -> i64 {
    if evicted.op_count() == 0 {
        return 0;
    }
    let numerator = (evicted.inclusion_fee().as_i64() as i128).saturating_mul(tx_ops as i128);
    let denominator = evicted.op_count() as i128;
    let base = numerator / denominator;
    let candidate = base.saturating_add(1);
    i64::try_from(candidate).unwrap_or(i64::MAX)
}

fn min_inclusion_fee_to_beat(evicted: Option<&FeeRate>, tx_fee_rate: &FeeRate) -> i64 {
    let evicted = match evicted {
        Some(e) => e,
        None => return 0,
    };
    if evicted.op_count() == 0 {
        return 0;
    }
    if evicted.cmp_rate(tx_fee_rate) != Ordering::Less {
        compute_better_fee(evicted, tx_fee_rate.op_count())
    } else {
        0
    }
}

/// Check if a fee-bump transaction can replace an existing transaction.
/// For replace-by-fee to work, the new fee must be at least FEE_MULTIPLIER times the old fee rate.
/// Returns Ok(()) if replacement is allowed, or Err(min_fee) if the fee is insufficient.
fn can_replace_by_fee(new: &FeeRate, old: &FeeRate) -> std::result::Result<(), i64> {
    let new_fee = new.inclusion_fee().as_i64();
    let new_ops = new.op_count();
    let old_fee = old.inclusion_fee().as_i64();
    let old_ops = old.op_count();
    // newFee / newOps >= FEE_MULTIPLIER * oldFee / oldOps
    // Cross-multiply to avoid division:
    // newFee * oldOps >= FEE_MULTIPLIER * oldFee * newOps
    let left = (new_fee as i128).saturating_mul(old_ops as i128);
    let right = (FEE_MULTIPLIER as i128)
        .saturating_mul(old_fee as i128)
        .saturating_mul(new_ops as i128);

    if left < right {
        // Calculate minimum fee required:
        // minFee * oldOps >= FEE_MULTIPLIER * oldFee * newOps
        // minFee >= (FEE_MULTIPLIER * oldFee * newOps) / oldOps + 1 (round up)
        let min_fee = if old_ops > 0 {
            let numerator = right;
            let denominator = old_ops as i128;
            let quotient = numerator / denominator;
            let remainder = numerator % denominator;
            let rounded = if remainder > 0 {
                quotient + 1
            } else {
                quotient
            };
            i64::try_from(rounded).unwrap_or(i64::MAX)
        } else {
            0
        };
        Err(min_fee)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(super) struct SelectedTxs {
    pub(super) transactions: Vec<crate::tx_set_utils::HashedTx>,
    pub(super) soroban_limited: bool,
    pub(super) dex_limited: bool,
    pub(super) classic_limited: bool,
}

/// Entry in the fee-ordered index. Sorted ascending by fee rate (using
/// cross-multiplication via `FeeRate::cmp_rate`), with reverse-hash tie-break
/// to match the existing `ensure_queue_capacity` eviction semantics.
#[derive(Clone, Eq, PartialEq, Debug)]
struct FeeEntry {
    fee_rate: FeeRate,
    hash: Hash256,
    is_dex: bool,
}

impl FeeEntry {
    fn from_queued(tx: &QueuedTransaction) -> Self {
        Self {
            fee_rate: tx.fee_rate,
            hash: tx.hash,
            is_dex: tx.is_dex,
        }
    }
}

impl Ord for FeeEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.fee_rate
            .cmp_rate(&other.fee_rate)
            .then_with(|| other.hash.0.cmp(&self.hash.0))
    }
}

impl PartialOrd for FeeEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Co-located transaction store with fee index. All mutations go through
/// helpers that maintain both structures atomically.
struct QueueStore {
    by_hash: HashMap<Hash256, QueuedTransaction>,
    fee_index: std::collections::BTreeSet<FeeEntry>,
    /// Persistent eviction queue for classic txs (DexLimitingLaneConfig).
    /// Lazy-initialized when classic lane config is available.
    classic_eviction_queue: Option<SurgePricingPriorityQueue>,
    /// Persistent eviction queue for soroban txs (SorobanGenericLaneConfig).
    /// Lazy-initialized when soroban resource limits are available.
    soroban_eviction_queue: Option<SurgePricingPriorityQueue>,
    /// Persistent eviction queue for global ops limit (OpsOnlyLaneConfig).
    /// Lazy-initialized when max_queue_ops is configured.
    global_ops_queue: Option<SurgePricingPriorityQueue>,
    /// Seed for eviction queue tie-breaking. Regenerated on clear() and shift()
    /// to match stellar-core's per-reset/per-ledger seed lifecycle.
    eviction_seed: u64,
    /// Persistent flood queue: populated on insert, drained by broadcast_with_visitor.
    /// Matches stellar-core's persistent `mTxsToFlood` inside `TxQueueLimiter`.
    flood_queue: flood_queue::FloodQueue,
}

impl QueueStore {
    fn new(has_dex_lane: bool) -> Self {
        let seed = if cfg!(test) {
            0
        } else {
            rand::thread_rng().gen()
        };
        Self {
            by_hash: HashMap::new(),
            fee_index: std::collections::BTreeSet::new(),
            classic_eviction_queue: None,
            soroban_eviction_queue: None,
            global_ops_queue: None,
            eviction_seed: seed,
            flood_queue: flood_queue::FloodQueue::new(has_dex_lane),
        }
    }

    fn insert(&mut self, tx: QueuedTransaction, ledger_version: u32) {
        let entry = FeeEntry::from_queued(&tx);

        // Update eviction queues before inserting into by_hash.
        let is_soroban = henyey_tx::envelope_utils::is_soroban_envelope(&tx.envelope);

        if is_soroban {
            if let Some(ref mut queue) = self.soroban_eviction_queue {
                queue.add(tx.clone(), ledger_version);
            }
        } else if let Some(ref mut queue) = self.classic_eviction_queue {
            queue.add(tx.clone(), ledger_version);
        }
        if let Some(ref mut queue) = self.global_ops_queue {
            queue.add(tx.clone(), ledger_version);
        }

        // Mark for flooding (persistent flood queue — matches stellar-core's
        // markTxForFlood on addTransaction).
        self.flood_queue.mark_for_flood(&tx, ledger_version);

        self.by_hash.insert(tx.hash, tx);
        self.fee_index.insert(entry);
    }

    /// Remove a transaction by hash (pure storage operation).
    ///
    /// Does NOT reset eviction thresholds. Queue-shrinking callers (ban,
    /// evict_expired, remove_applied) must reset thresholds explicitly after
    /// removal — use a `did_remove` flag with `eviction_thresholds.reset_all()`
    /// after the batch. Admission-path callers (try_add, ensure_queue_capacity)
    /// should NOT reset because thresholds were freshly computed by
    /// `record_lane_evictions`.
    fn remove(&mut self, hash: &Hash256, ledger_version: u32) -> Option<QueuedTransaction> {
        if let Some(tx) = self.by_hash.remove(hash) {
            self.fee_index.remove(&FeeEntry::from_queued(&tx));

            // Remove from eviction queues.
            let entry = QueueEntry::new(tx.clone(), self.eviction_seed);
            let is_soroban = henyey_tx::envelope_utils::is_soroban_envelope(&tx.envelope);

            if is_soroban {
                if let Some(ref mut queue) = self.soroban_eviction_queue {
                    let lane = queue.get_lane(&tx.envelope);
                    queue.remove_entry(lane, &entry, ledger_version);
                }
            } else if let Some(ref mut queue) = self.classic_eviction_queue {
                let lane = queue.get_lane(&tx.envelope);
                queue.remove_entry(lane, &entry, ledger_version);
            }
            if let Some(ref mut queue) = self.global_ops_queue {
                let lane = queue.get_lane(&tx.envelope);
                queue.remove_entry(lane, &entry, ledger_version);
            }

            // Remove from flood queue (matches stellar-core: removal paths
            // erase from mTxsToFlood).
            self.flood_queue.remove(&tx, ledger_version);

            Some(tx)
        } else {
            None
        }
    }

    /// Clear only the transaction data (by_hash + fee_index).
    /// Does NOT invalidate eviction queues — the caller is responsible for
    /// calling `regenerate_eviction_seed()` or using a `TransactionQueue`
    /// invalidation helper.
    fn clear_data(&mut self) {
        self.by_hash.clear();
        self.fee_index.clear();
        self.flood_queue.clear();
    }

    /// Regenerate the eviction seed and invalidate all persistent eviction
    /// queues. They will be lazily rebuilt with the new seed on next admission.
    /// Parity: stellar-core regenerates the tie-break seed in shift() and
    /// creates new queues with fresh seeds in TxQueueLimiter::reset().
    fn regenerate_eviction_seed(&mut self) {
        self.eviction_seed = if cfg!(test) {
            0
        } else {
            rand::thread_rng().gen()
        };
        self.classic_eviction_queue = None;
        self.soroban_eviction_queue = None;
        self.global_ops_queue = None;
    }

    fn get(&self, hash: &Hash256) -> Option<&QueuedTransaction> {
        self.by_hash.get(hash)
    }

    fn contains_key(&self, hash: &Hash256) -> bool {
        self.by_hash.contains_key(hash)
    }

    fn len(&self) -> usize {
        self.by_hash.len()
    }

    fn is_empty(&self) -> bool {
        self.by_hash.is_empty()
    }

    fn values(&self) -> impl Iterator<Item = &QueuedTransaction> {
        self.by_hash.values()
    }

    #[cfg(any(test, feature = "test-utils"))]
    #[allow(dead_code)]
    fn values_mut(&mut self) -> impl Iterator<Item = &mut QueuedTransaction> {
        self.by_hash.values_mut()
    }

    fn iter(&self) -> impl Iterator<Item = (&Hash256, &QueuedTransaction)> {
        self.by_hash.iter()
    }

    /// Peek the lowest-fee entry from the index. O(log n).
    fn lowest_fee(&self) -> Option<&FeeEntry> {
        self.fee_index.iter().next()
    }

    /// Debug assertion: verify fee_index ↔ by_hash consistency.
    #[cfg(test)]
    #[allow(dead_code)]
    fn assert_consistent(&self) {
        assert_eq!(
            self.by_hash.len(),
            self.fee_index.len(),
            "QueueStore: by_hash.len() != fee_index.len()"
        );
        for (hash, tx) in &self.by_hash {
            let entry = FeeEntry::from_queued(tx);
            assert!(
                self.fee_index.contains(&entry),
                "QueueStore: tx {:?} in by_hash but not in fee_index",
                hash
            );
        }
    }

    /// Verify persistent eviction queues match a cold rebuild from by_hash.
    ///
    /// For each active eviction queue, rebuilds a fresh queue from scratch and
    /// compares total/per-lane resource counts and ordered entry hashes.
    #[cfg(test)]
    #[allow(dead_code)]
    fn assert_eviction_queues_consistent(&self, ledger_version: u32) {
        // Check classic queue
        if let Some(ref queue) = self.classic_eviction_queue {
            let mut fresh = SurgePricingPriorityQueue::new(
                Box::new(DexLimitingLaneConfig::new(
                    queue.lane_limits(GENERIC_LANE),
                    if queue.get_num_lanes() > 1 {
                        Some(queue.lane_limits(1))
                    } else {
                        None
                    },
                )),
                self.eviction_seed,
            );
            for tx in self.by_hash.values() {
                if !henyey_tx::envelope_utils::is_soroban_envelope(&tx.envelope) {
                    fresh.add(tx.clone(), ledger_version);
                }
            }
            assert_eq!(
                queue.total_resources(),
                fresh.total_resources(),
                "classic eviction queue total resources mismatch"
            );
            for lane in 0..queue.get_num_lanes() {
                assert_eq!(
                    queue.lane_resources(lane),
                    fresh.lane_resources(lane),
                    "classic eviction queue lane {lane} resources mismatch"
                );
                assert_eq!(
                    queue.lane_entry_hashes(lane),
                    fresh.lane_entry_hashes(lane),
                    "classic eviction queue lane {lane} entries mismatch"
                );
            }
        }

        // Check soroban queue
        if let Some(ref queue) = self.soroban_eviction_queue {
            let mut fresh = SurgePricingPriorityQueue::new(
                Box::new(SorobanGenericLaneConfig::new(
                    queue.lane_limits(GENERIC_LANE),
                )),
                self.eviction_seed,
            );
            for tx in self.by_hash.values() {
                if henyey_tx::envelope_utils::is_soroban_envelope(&tx.envelope) {
                    fresh.add(tx.clone(), ledger_version);
                }
            }
            assert_eq!(
                queue.total_resources(),
                fresh.total_resources(),
                "soroban eviction queue total resources mismatch"
            );
            for lane in 0..queue.get_num_lanes() {
                assert_eq!(
                    queue.lane_entry_hashes(lane),
                    fresh.lane_entry_hashes(lane),
                    "soroban eviction queue lane {lane} entries mismatch"
                );
            }
        }

        // Check global ops queue
        if let Some(ref queue) = self.global_ops_queue {
            let mut fresh = SurgePricingPriorityQueue::new(
                Box::new(OpsOnlyLaneConfig::new(queue.lane_limits(GENERIC_LANE))),
                self.eviction_seed,
            );
            for tx in self.by_hash.values() {
                fresh.add(tx.clone(), ledger_version);
            }
            assert_eq!(
                queue.total_resources(),
                fresh.total_resources(),
                "global ops eviction queue total resources mismatch"
            );
            for lane in 0..queue.get_num_lanes() {
                assert_eq!(
                    queue.lane_entry_hashes(lane),
                    fresh.lane_entry_hashes(lane),
                    "global ops eviction queue lane {lane} entries mismatch"
                );
            }
        }
    }

    /// Ensure the classic eviction queue exists, building it from scratch if needed.
    fn ensure_classic_queue(
        &mut self,
        lane_config: DexLimitingLaneConfig,
        ledger_version: u32,
    ) -> &SurgePricingPriorityQueue {
        if self.classic_eviction_queue.is_none() {
            let mut queue =
                SurgePricingPriorityQueue::new(Box::new(lane_config), self.eviction_seed);
            for tx in self.by_hash.values() {
                if !henyey_tx::envelope_utils::is_soroban_envelope(&tx.envelope) {
                    queue.add(tx.clone(), ledger_version);
                }
            }
            self.classic_eviction_queue = Some(queue);
        }
        self.classic_eviction_queue.as_ref().unwrap()
    }

    /// Ensure the soroban eviction queue exists, building it from scratch if needed.
    fn ensure_soroban_queue(
        &mut self,
        limit: Resource,
        ledger_version: u32,
    ) -> &SurgePricingPriorityQueue {
        if self.soroban_eviction_queue.is_none() {
            let lane_config = SorobanGenericLaneConfig::new(limit);
            let mut queue =
                SurgePricingPriorityQueue::new(Box::new(lane_config), self.eviction_seed);
            for tx in self.by_hash.values() {
                if henyey_tx::envelope_utils::is_soroban_envelope(&tx.envelope) {
                    queue.add(tx.clone(), ledger_version);
                }
            }
            self.soroban_eviction_queue = Some(queue);
        }
        self.soroban_eviction_queue.as_ref().unwrap()
    }

    /// Ensure the global ops eviction queue exists, building it from scratch if needed.
    fn ensure_global_ops_queue(
        &mut self,
        limit: i64,
        ledger_version: u32,
    ) -> &SurgePricingPriorityQueue {
        if self.global_ops_queue.is_none() {
            let lane_config = OpsOnlyLaneConfig::new(Resource::new(vec![limit]));
            let mut queue =
                SurgePricingPriorityQueue::new(Box::new(lane_config), self.eviction_seed);
            for tx in self.by_hash.values() {
                queue.add(tx.clone(), ledger_version);
            }
            self.global_ops_queue = Some(queue);
        }
        self.global_ops_queue.as_ref().unwrap()
    }
}

/// Bundled properties of a transaction being evaluated for queue admission.
struct EvictionCandidate<'a> {
    queued: &'a QueuedTransaction,
    is_soroban: bool,
    ledger_version: u32,
}

/// Build an `EvictionExclusion` for a persistent eviction queue query.
///
/// Excludes the RBF-replaced tx (if any) and any cross-queue evictions from
/// prior passes, adjusting per-lane resource discounts accordingly.
fn build_eviction_exclusion(
    queue: &SurgePricingPriorityQueue,
    by_hash: &HashMap<Hash256, QueuedTransaction>,
    replaced_tx: Option<&QueuedTransaction>,
    cross_queue_evictions: &HashSet<Hash256>,
    ledger_version: u32,
) -> EvictionExclusion {
    let num_lanes = queue.lane_count();
    let resource_dim = queue.resource_dim();
    let mut excl = EvictionExclusion::new(num_lanes, resource_dim);

    // Add replaced tx (RBF). stellar-core subtracts the old tx's resources
    // before calling canFitWithEviction.
    if let Some(old_tx) = replaced_tx {
        excl.hashes.insert(old_tx.hash);
        let lane = queue.get_lane(&old_tx.envelope);
        let resources = queue.tx_resources(&old_tx.envelope, ledger_version);
        excl.lane_resource_discount[lane] = excl.lane_resource_discount[lane].clone() + resources;
    }

    // Add cross-queue evictions from prior passes
    for hash in cross_queue_evictions {
        if excl.hashes.insert(*hash) {
            if let Some(tx) = by_hash.get(hash) {
                let lane = queue.get_lane(&tx.envelope);
                let resources = queue.tx_resources(&tx.envelope, ledger_version);
                excl.lane_resource_discount[lane] =
                    excl.lane_resource_discount[lane].clone() + resources;
            }
        }
    }

    excl
}

/// Get the source account (inner for fee-bump) as a MuxedAccount.
fn source_account_from_envelope(envelope: &TransactionEnvelope) -> stellar_xdr::MuxedAccount {
    match envelope {
        TransactionEnvelope::TxV0(env) => {
            stellar_xdr::MuxedAccount::Ed25519(env.tx.source_account_ed25519.clone())
        }
        TransactionEnvelope::Tx(env) => env.tx.source_account.clone(),
        TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
            stellar_xdr::FeeBumpTransactionInnerTx::Tx(inner) => inner.tx.source_account.clone(),
        },
    }
}

fn account_key(envelope: &TransactionEnvelope) -> Vec<u8> {
    let account_id = henyey_tx::muxed_to_account_id(&source_account_from_envelope(envelope));
    xdr_to_bytes(&account_id)
}

pub(crate) fn account_key_from_account_id(account_id: &AccountId) -> Vec<u8> {
    xdr_to_bytes(account_id)
}

fn account_id_from_envelope(envelope: &TransactionEnvelope) -> AccountId {
    henyey_tx::muxed_to_account_id(&source_account_from_envelope(envelope))
}

/// Get the fee-source account key (for fee bump, this is the outer source; otherwise same as inner).
fn fee_source_key(envelope: &TransactionEnvelope) -> Vec<u8> {
    let fee_source = match envelope {
        TransactionEnvelope::TxV0(env) => {
            stellar_xdr::MuxedAccount::Ed25519(env.tx.source_account_ed25519.clone())
        }
        TransactionEnvelope::Tx(env) => env.tx.source_account.clone(),
        TransactionEnvelope::TxFeeBump(env) => env.tx.fee_source.clone(),
    };
    let account_id = henyey_tx::muxed_to_account_id(&fee_source);
    xdr_to_bytes(&account_id)
}

/// Check if envelope is a fee-bump transaction.
use henyey_tx::envelope_utils::is_fee_bump_envelope;

/// Convert an XDR-encoded account key back to AccountId.
fn account_id_from_fee_source_key(key: &[u8]) -> AccountId {
    use stellar_xdr::ReadXdr;
    AccountId::from_xdr(key, Limits::none()).unwrap_or({
        // Fallback to a zero account ID if decoding fails
        AccountId(stellar_xdr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::Uint256([0; 32]),
        ))
    })
}

/// Queue of pending transactions.
///
/// Maintains transactions waiting to be included in a ledger, ordered by fee.
///
/// # Per-Account Limits
///
/// Parity: The queue enforces one transaction per account (sequence-number-source).
/// This prevents spam and ensures predictable transaction ordering. Accounts can
/// replace their pending transaction with a fee-bump (10x fee multiplier required).
///
/// # Transaction Aging
///
/// Transactions that sit in the queue for too long (pending_depth ledgers) are
/// automatically banned. This prevents stale transactions from occupying queue space.
/// Cached min-fee thresholds from the most recent eviction pass.
/// Used for fast-path admission rejection without rebuilding eviction queues.
///
/// These thresholds live in `TransactionQueue` (not `QueueStore`) because
/// they're read during the admission fast-path under separate `RwLock`s
/// without holding the store lock.
struct EvictionThresholds {
    /// Lane eviction thresholds for classic queue admission.
    classic_lane_fees: RwLock<Vec<Option<FeeRate>>>,
    /// Lane eviction thresholds for Soroban queue admission.
    soroban_lane_fees: RwLock<Vec<Option<FeeRate>>>,
    /// Eviction threshold for global queue limits.
    global_fees: RwLock<Option<FeeRate>>,
}

impl EvictionThresholds {
    fn new() -> Self {
        Self {
            classic_lane_fees: RwLock::new(Vec::new()),
            soroban_lane_fees: RwLock::new(Vec::new()),
            global_fees: RwLock::new(None),
        }
    }

    /// Reset all cached thresholds.
    fn reset_all(&self) {
        self.classic_lane_fees.write().clear();
        self.soroban_lane_fees.write().clear();
        *self.global_fees.write() = None;
    }

    /// Reset only Soroban lane thresholds.
    fn reset_soroban(&self) {
        self.soroban_lane_fees.write().clear();
    }

    /// Reset only the global ops eviction threshold. Used when the classic ops
    /// capacity changes (#3612) so a stale fee floor computed against the old
    /// capacity does not reject admissions under the new one.
    fn reset_global_ops(&self) {
        *self.global_fees.write() = None;
    }
}

/// ## Lock Ordering (MUST be followed by all methods)
///
/// When acquiring multiple write locks, always acquire in this order:
///
///   `store → account_states → banned_transactions → seen → arb_damper`
///
/// `validation_context` is excluded: it is only read-locked in
/// multi-lock contexts. If a future change needs `.write()` while
/// holding other locks, add it to this order.
///
/// Violating this order creates ABBA deadlock risk between concurrent
/// callers (e.g., RPC `try_add` vs spawn_blocking `remove_applied`).
/// See issue #1930 for the deadlock that motivated this invariant.
pub struct TransactionQueue {
    /// Configuration.
    config: TxQueueConfig,
    /// Transactions indexed by hash with co-located fee index.
    store: RwLock<QueueStore>,
    /// Seen transaction hashes (includes recently applied).
    seen: RwLock<HashSet<Hash256>>,
    /// Validation context (ledger state info for validation).
    validation_context: RwLock<ValidationContext>,
    /// Cached eviction fee thresholds for fast-path admission rejection.
    eviction_thresholds: EvictionThresholds,
    /// Banned transaction hashes, organized as a deque of sets.
    /// Each set represents one ledger's worth of banned transactions.
    /// The front is the oldest, the back is the newest.
    banned_transactions: RwLock<std::collections::VecDeque<HashSet<Hash256>>>,
    /// Per-account state tracking for one-tx-per-account limit.
    /// Key is the XDR-encoded AccountId bytes.
    account_states: RwLock<HashMap<Vec<u8>, AccountState>>,
    /// Number of ledgers before auto-banning stale transactions.
    pending_depth: u32,
    /// Optional fee balance provider for validating fee-source balances.
    /// When set, transactions are validated to ensure the fee-source has
    /// sufficient balance to cover all pending fees plus the new transaction fee.
    fee_balance_provider: RwLock<Option<Arc<dyn FeeBalanceProvider>>>,
    /// Optional account provider for tx-set validation (sequence + auth checks).
    /// When set, tx-set validation verifies account existence, sequence numbers,
    /// and signatures — matching stellar-core's `getInvalidTxListWithErrors`.
    account_provider: RwLock<Option<Arc<dyn AccountProvider>>>,
    /// Test-only: when true, skip fee balance validation in try_add.
    /// Matches stellar-core's `isLoadgenTx` bypass in TransactionQueue::canAdd()
    /// which skips both tx validation and fee balance checks for loadgen txs
    /// (gated on BUILD_TESTS / #ifdef BUILD_TESTS).
    #[cfg(any(test, feature = "test-utils"))]
    skip_fee_balance_check: std::sync::atomic::AtomicBool,
    /// Dynamic Soroban resource limits, updated after each ledger close from
    /// `SorobanNetworkInfo`.  Takes precedence over `config.max_queue_soroban_resources`.
    dynamic_queue_soroban_resources: RwLock<Option<Resource>>,
    /// Dynamic Soroban resource limits for tx-set selection (1x ledger max).
    /// Separate from queue-admission limits which apply
    /// `SOROBAN_TRANSACTION_QUEUE_SIZE_MULTIPLIER` (2x).
    dynamic_selection_soroban_resources: RwLock<Option<Resource>>,
    /// Dynamic ledger-wide max Soroban instructions, re-derived from the live
    /// LCL `SorobanNetworkConfig` (ContractComputeV0) on every ledger close.
    /// Takes precedence over the construction-time `config.ledger_max_instructions`.
    ///
    /// Parity: stellar-core builds the parallel Soroban phase from the LIVE
    /// config via `getLastClosedSorobanNetworkConfig().ledgerMaxInstructions()`
    /// (`TxSetFrame.cpp:606-608`, `ParallelTxSetBuilder.cpp`). Henyey froze this
    /// at app construction (prod default 0), so `use_parallel` was never enabled
    /// after a Soroban ConfigUpgrade. See #3680.
    dynamic_ledger_max_instructions: RwLock<Option<i64>>,
    /// Dynamic ledger-wide max dependent tx clusters per stage, re-derived from
    /// the live LCL `SorobanNetworkConfig` (ContractParallelComputeV0) on every
    /// ledger close. Takes precedence over
    /// `config.ledger_max_dependent_tx_clusters`.
    ///
    /// Parity: stellar-core reads this live via
    /// `ledgerMaxDependentTxClusters()`. See #3680.
    dynamic_ledger_max_dependent_tx_clusters: RwLock<Option<u32>>,
    /// Dynamic classic queue ops capacity (already scaled by
    /// POOL_LEDGER_MULTIPLIER), re-derived from the live ledger header's
    /// `maxTxSetSize` on every ledger close. Takes precedence over the
    /// construction-time `config.max_queue_ops` / `config.max_size`.
    ///
    /// Parity: stellar-core `TxQueueLimiter::reset(ledgerVersion)` rebuilds the
    /// classic generic-lane capacity from `maxScaledLedgerResources(false)` =
    /// `maxLedgerResources(false) * mPoolLedgerMultiplier`
    /// (`TxQueueLimiter.cpp:61,244`), reading the live `maxTxSetSize` each close.
    /// Henyey froze the capacity at app construction, so `UpgradeMaxTxSetSize`
    /// was never tracked. See #3612.
    dynamic_max_queue_ops: RwLock<Option<u32>>,
    /// Arbitrage flood damper. Acquired after `store` lock in `broadcast_with_visitor`
    /// and `shift()`. Cleared by `shift()`, preserved by `reset_and_rebuild()`.
    arb_damper: parking_lot::Mutex<arb_flood_damping::ArbitrageFloodDamper>,
    /// Counter: arb txs seen during broadcast (has payment loops).
    arb_tx_seen: std::sync::atomic::AtomicU64,
    /// Counter: arb txs dampened during broadcast (not broadcast).
    arb_tx_dropped: std::sync::atomic::AtomicU64,
}

/// Default ban depth (number of ledgers transactions stay banned).
/// Spec: HERDER_SPEC §11.7 — TRANSACTION_QUEUE_BAN_LEDGERS = 10.
/// Parity: stellar-core `HerderImpl.cpp:66` —
/// `constexpr uint32 const TRANSACTION_QUEUE_BAN_LEDGERS = 10;`.
const TRANSACTION_QUEUE_BAN_LEDGERS: u32 = 10;

impl TransactionQueue {
    /// Create a new transaction queue.
    pub fn new(config: TxQueueConfig) -> Self {
        Self::with_depths(
            config,
            TRANSACTION_QUEUE_BAN_LEDGERS,
            TRANSACTION_QUEUE_TIMEOUT_LEDGERS,
        )
    }

    /// Create a new transaction queue with custom ban depth.
    pub fn with_ban_depth(config: TxQueueConfig, ban_depth: u32) -> Self {
        Self::with_depths(config, ban_depth, TRANSACTION_QUEUE_TIMEOUT_LEDGERS)
    }

    /// Create a new transaction queue with custom ban and pending depths.
    ///
    /// # Arguments
    ///
    /// * `config` - Queue configuration
    /// * `ban_depth` - Number of ledgers transactions stay banned
    /// * `pending_depth` - Number of ledgers before stale transactions are auto-banned
    pub fn with_depths(config: TxQueueConfig, ban_depth: u32, pending_depth: u32) -> Self {
        let ctx = ValidationContext {
            base_fee: config.min_fee_per_op,
            expected_close_time: Duration::from_secs(config.expected_ledger_close_secs),
            ..Default::default()
        };

        // Initialize the banned transactions deque with ban_depth empty sets
        let mut banned = std::collections::VecDeque::with_capacity(ban_depth as usize);
        for _ in 0..ban_depth {
            banned.push_back(HashSet::new());
        }

        let arb_damper = arb_flood_damping::ArbitrageFloodDamper::new(
            config.flood_arb_tx_base_allowance,
            config.flood_arb_tx_damping_factor,
        );

        let has_dex_lane = config.max_dex_ops.is_some();

        Self {
            store: RwLock::new(QueueStore::new(has_dex_lane)),
            config,
            seen: RwLock::new(HashSet::new()),
            validation_context: RwLock::new(ctx),
            eviction_thresholds: EvictionThresholds::new(),
            banned_transactions: RwLock::new(banned),
            account_states: RwLock::new(HashMap::new()),
            pending_depth,
            fee_balance_provider: RwLock::new(None),
            account_provider: RwLock::new(None),
            #[cfg(any(test, feature = "test-utils"))]
            skip_fee_balance_check: std::sync::atomic::AtomicBool::new(false),
            dynamic_queue_soroban_resources: RwLock::new(None),
            dynamic_selection_soroban_resources: RwLock::new(None),
            dynamic_ledger_max_instructions: RwLock::new(None),
            dynamic_ledger_max_dependent_tx_clusters: RwLock::new(None),
            dynamic_max_queue_ops: RwLock::new(None),
            arb_damper: parking_lot::Mutex::new(arb_damper),
            arb_tx_seen: std::sync::atomic::AtomicU64::new(0),
            arb_tx_dropped: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Set the fee balance provider for validating fee-source balances.
    ///
    /// When set, transactions are validated to ensure the fee-source account
    /// has sufficient balance to cover all pending fees plus the new transaction fee.
    pub fn set_fee_balance_provider(&self, provider: Arc<dyn FeeBalanceProvider>) {
        *self.fee_balance_provider.write() = Some(provider);
    }

    /// Clear the fee balance provider.
    pub fn clear_fee_balance_provider(&self) {
        *self.fee_balance_provider.write() = None;
    }

    /// Get the fee balance provider (for post-close invalidation).
    pub fn get_fee_balance_provider(&self) -> Option<Arc<dyn FeeBalanceProvider>> {
        self.fee_balance_provider.read().clone()
    }

    /// Set the account provider for tx-set validation.
    pub fn set_account_provider(&self, provider: Arc<dyn AccountProvider>) {
        *self.account_provider.write() = Some(provider);
    }

    /// Get the account provider (for tx-set building).
    pub fn get_account_provider(&self) -> Option<Arc<dyn AccountProvider>> {
        self.account_provider.read().clone()
    }

    /// Return all queued transaction envelopes (for post-close invalidation).
    pub fn pending_envelopes(&self) -> Vec<TransactionEnvelope> {
        let store = self.store.read();
        store
            .values()
            .map(|qt| Arc::unwrap_or_clone(qt.envelope.clone()))
            .collect()
    }

    /// Return all queued transactions as pre-hashed pairs (Phase 6 optimization).
    ///
    /// Avoids redundant `Hash256::hash_xdr()` in the post-close invalidation
    /// path by reusing the hash computed at queue admission time.
    pub fn pending_hashed_envelopes(&self) -> Vec<crate::tx_set_utils::HashedTx> {
        let store = self.store.read();
        store
            .values()
            .map(crate::tx_set_utils::HashedTx::from)
            .collect()
    }

    /// Update Soroban resource limits dynamically after ledger close.
    ///
    /// Called with limits derived from `SorobanNetworkInfo` multiplied by
    /// `SOROBAN_TRANSACTION_QUEUE_SIZE_MULTIPLIER`.
    pub fn update_soroban_resource_limits(&self, resources: Resource) {
        *self.dynamic_queue_soroban_resources.write() = Some(resources);
        // Invalidate Soroban eviction state: persistent queue + cached thresholds.
        self.invalidate_soroban_eviction_state(&mut self.store.write());
    }

    /// Update Soroban resource limits for tx-set selection (1x ledger max).
    /// Called alongside `update_soroban_resource_limits` but without the
    /// `SOROBAN_TRANSACTION_QUEUE_SIZE_MULTIPLIER` scaling.
    pub fn update_soroban_selection_limits(&self, resources: Resource) {
        *self.dynamic_selection_soroban_resources.write() = Some(resources);
    }

    /// Return the effective Soroban resource limits for tx-set selection.
    /// Uses the 1x ledger-max dynamic value, falling back to the config value.
    pub fn effective_selection_soroban_resources(&self) -> Option<Resource> {
        let dynamic = self.dynamic_selection_soroban_resources.read();
        if dynamic.is_some() {
            dynamic.clone()
        } else {
            self.config.max_soroban_resources.clone()
        }
    }

    /// Update the ledger-wide parallel-Soroban phase limits from the live LCL
    /// `SorobanNetworkConfig`. Called on every ledger close alongside
    /// `update_soroban_selection_limits`.
    ///
    /// Parity: stellar-core builds the surge-priced parallel Soroban phase from
    /// the live config (`getLastClosedSorobanNetworkConfig()`), reading
    /// `ledgerMaxInstructions()` and `ledgerMaxDependentTxClusters()` each close
    /// (`TxSetFrame.cpp:606-608`). Without this refresh henyey's `use_parallel`
    /// gate stayed at the construction-time default (0/0) and never built a
    /// parallel phase after a Soroban ConfigUpgrade. See #3680.
    pub fn update_soroban_parallel_limits(&self, ledger_max_instructions: i64, clusters: u32) {
        *self.dynamic_ledger_max_instructions.write() = Some(ledger_max_instructions);
        *self.dynamic_ledger_max_dependent_tx_clusters.write() = Some(clusters);
    }

    /// Effective ledger-wide max Soroban instructions for parallel-phase
    /// building. Prefers the dynamic value (refreshed from live LCL config each
    /// close) over the construction-time config.
    pub fn effective_ledger_max_instructions(&self) -> i64 {
        self.dynamic_ledger_max_instructions
            .read()
            .unwrap_or(self.config.ledger_max_instructions)
    }

    /// Effective ledger-wide max dependent tx clusters per stage for
    /// parallel-phase building. Prefers the dynamic value (refreshed from live
    /// LCL config each close) over the construction-time config.
    pub fn effective_ledger_max_dependent_tx_clusters(&self) -> u32 {
        self.dynamic_ledger_max_dependent_tx_clusters
            .read()
            .unwrap_or(self.config.ledger_max_dependent_tx_clusters)
    }

    /// Return the effective Soroban resource limits for queue admission.
    /// Prefers the dynamic value (updated each ledger close) over the static config.
    pub(crate) fn effective_queue_soroban_resources(&self) -> Option<Resource> {
        let dynamic = self.dynamic_queue_soroban_resources.read();
        if dynamic.is_some() {
            dynamic.clone()
        } else {
            self.config.max_queue_soroban_resources.clone()
        }
    }

    /// Update the classic queue ops capacity from the live ledger header.
    ///
    /// `scaled_max_queue_ops` is the live `maxTxSetSize` already multiplied by
    /// the pool ledger multiplier (the caller scales, mirroring how
    /// [`update_soroban_resource_limits`](Self::update_soroban_resource_limits)
    /// receives a pre-scaled `Resource`). Called on every ledger close so the
    /// classic limiter tracks `UpgradeMaxTxSetSize`.
    ///
    /// Parity: stellar-core `TxQueueLimiter::reset(ledgerVersion)` rebuilds the
    /// generic-lane capacity from `maxScaledLedgerResources(false)`
    /// (`TxQueueLimiter.cpp:244`), which reads the live `maxTxSetSize` and scales
    /// by `mPoolLedgerMultiplier`. The persistent global-ops eviction queue is
    /// rebuilt lazily with the new limit, and the stale global-ops fee floor is
    /// cleared so it cannot reject admissions against the old capacity. See #3612.
    pub fn update_classic_queue_capacity(&self, scaled_max_queue_ops: u32) {
        *self.dynamic_max_queue_ops.write() = Some(scaled_max_queue_ops);
        // Drop the persistent global-ops queue so it rebuilds with the new
        // limit on the next admission, and clear the stale global fee floor.
        self.store.write().global_ops_queue = None;
        self.eviction_thresholds.reset_global_ops();
    }

    /// Return the effective classic queue ops capacity.
    ///
    /// Prefers the dynamic value (re-derived from the live `maxTxSetSize` each
    /// ledger close via [`update_classic_queue_capacity`](Self::update_classic_queue_capacity))
    /// over the construction-time `config.max_queue_ops`. Returns `None` only
    /// when neither is configured (classic ops gate disabled). See #3612.
    pub fn effective_max_queue_ops(&self) -> Option<u32> {
        let dynamic = *self.dynamic_max_queue_ops.read();
        dynamic.or(self.config.max_queue_ops)
    }

    /// Return the effective classic queue tx-count cap.
    ///
    /// Tracks the live `maxTxSetSize` (scaled) when a dynamic capacity is set,
    /// falling back to the construction-time `config.max_size`. Keeps the
    /// tx-count guard consistent with the ops capacity after an
    /// `UpgradeMaxTxSetSize`. See #3612.
    pub fn effective_max_size(&self) -> usize {
        match *self.dynamic_max_queue_ops.read() {
            Some(ops) => ops as usize,
            None => self.config.max_size,
        }
    }

    /// Return the effective Soroban queue tx-count limit for flood demand sizing.
    ///
    /// Matches stellar-core `SorobanTransactionQueue::getMaxQueueSizeOps()`:
    /// returns the `Operations` slot of the pool-scaled Soroban resource limits,
    /// or `0` when no Soroban limits are configured.
    pub fn max_queue_size_soroban_ops(&self) -> usize {
        self.effective_queue_soroban_resources()
            .and_then(|r| r.try_get_val(ResourceType::Operations))
            .map(|ops| usize::try_from(ops.max(0)).unwrap_or(usize::MAX))
            .unwrap_or(0)
    }

    /// Test-only: skip fee balance validation in try_add.
    ///
    /// Matches stellar-core's `isLoadgenTx` bypass which skips both tx validation
    /// and fee balance checks for loadgen transactions under `#ifdef BUILD_TESTS`.
    /// In simulation tests, the pair topology may not execute create-account txs
    /// before loadgen payments are submitted, so the fee source accounts may not
    /// exist in the bucket list yet.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn set_skip_fee_balance_check(&self, skip: bool) {
        self.skip_fee_balance_check
            .store(skip, std::sync::atomic::Ordering::Relaxed);
    }

    /// Test-only: insert an envelope directly into the queue, bypassing
    /// all admission validation.
    ///
    /// Used by regression tests that need to drive code paths (e.g.,
    /// post-close re-validation) over an arbitrary queue population
    /// without constructing fully-valid signed envelopes. Matches the
    /// behaviour of `try_add` on success but performs no checks —
    /// callers are responsible for providing a structurally valid
    /// envelope.
    ///
    /// Returns `true` if insertion succeeded, `false` if the envelope
    /// failed to parse fees/ops.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn insert_for_test(&self, envelope: TransactionEnvelope) -> bool {
        let Ok(queued) = QueuedTransaction::new(envelope) else {
            return false;
        };
        let hash = queued.hash;
        let ledger_version = self.validation_context.read().protocol_version;
        self.store.write().insert(queued, ledger_version);
        self.seen.write().insert(hash);
        true
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(TxQueueConfig::default())
    }

    /// Create with a maximum size.
    pub fn with_max_size(max_size: usize) -> Self {
        Self::new(TxQueueConfig {
            max_size,
            ..Default::default()
        })
    }

    /// Update the validation context (should be called when ledger closes).
    #[allow(clippy::too_many_arguments)]
    pub fn update_validation_context(
        &self,
        ledger_seq: u32,
        close_time: u64,
        protocol_version: u32,
        base_fee: u32,
        base_reserve: u32,
        ledger_flags: u32,
        expected_close_time: Duration,
    ) {
        let mut ctx = self.validation_context.write();
        ctx.ledger_seq = ledger_seq;
        ctx.close_time = close_time;
        ctx.protocol_version = protocol_version;
        ctx.base_fee = base_fee;
        ctx.base_reserve = base_reserve;
        ctx.ledger_flags = ledger_flags;
        ctx.expected_close_time = expected_close_time;
    }

    /// Set the Soroban per-transaction resource limits in the validation context.
    ///
    /// Called during startup seeding (before the first ledger close) to ensure
    /// Soroban txs are validated against network config limits from the start.
    pub fn set_soroban_limits(&self, limits: SorobanTxLimits) {
        self.validation_context.write().soroban_limits = Some(limits);
    }

    /// Set the per-TX Soroban resource limits in the validation context.
    pub fn set_soroban_resource_limits(&self, limits: henyey_tx::SorobanResourceLimits) {
        self.validation_context.write().soroban_resource_limits = Some(limits);
    }

    /// Early cross-type source-account conflict check.
    ///
    /// Parity: stellar-core `HerderImpl::recvTransaction` (HerderImpl.cpp:627-645)
    /// rejects a tx whose sequence-number source already has a pending tx of the
    /// opposite type (classic vs Soroban) before ANY validation. This helper
    /// replicates that precedence so the externally visible result is
    /// `TryAgainLater` rather than whatever validation error would fire first.
    ///
    /// This is an optimistic read (no write lock); the later `check_account_limit`
    /// under the store write-lock remains the authoritative recheck.
    /// Check if a candidate transaction conflicts with an opposite-type pending
    /// entry for the same source account, using pre-computed key and type.
    ///
    /// Parity: stellar-core `HerderImpl::recvTransaction` (HerderImpl.cpp:627-645)
    /// checks `sourceAccountPending` on the opposite queue before `tryAdd`. This
    /// replicates that precedence so the externally visible result is
    /// `TryAgainLater` rather than whatever validation error would fire first.
    ///
    /// This is an optimistic read (no write lock); the later `check_account_limit`
    /// under the store write-lock remains the authoritative recheck.
    fn check_cross_type_conflict_with(
        &self,
        seq_source: &[u8],
        candidate_is_soroban: bool,
    ) -> Option<TxQueueResult> {
        let account_states = self.account_states.read();
        if let Some(state) = account_states.get(seq_source) {
            // Check if the account has a pending tx of the opposite type (seq-source role)
            if let Some(ref current_tx) = state.transaction {
                let current_is_soroban =
                    henyey_tx::envelope_utils::is_soroban_envelope(&current_tx.envelope);
                if current_is_soroban != candidate_is_soroban {
                    return Some(TxQueueResult::TryAgainLater);
                }
            }
            // Parity: stellar-core's `sourceAccountPending` returns true for
            // any entry in the opposite queue's `mAccountStates`, including
            // fee-source-only entries. Check if this account pays fees for a
            // tx of the opposite type.
            if candidate_is_soroban && state.classic_fee_tx_count > 0 {
                return Some(TxQueueResult::TryAgainLater);
            }
            if !candidate_is_soroban && state.soroban_fee_tx_count > 0 {
                return Some(TxQueueResult::TryAgainLater);
            }
        }
        None
    }

    /// Validate a transaction before queueing.
    fn validate_transaction(
        &self,
        envelope: &TransactionEnvelope,
    ) -> std::result::Result<(), henyey_tx::TxResultCode> {
        use henyey_tx::{
            is_too_early, is_too_late, validate_signatures, LedgerContext, TransactionFrame,
            TxResultCode,
        };

        let frame =
            TransactionFrame::from_owned_with_network(envelope.clone(), self.config.network_id);
        let ctx = self.validation_context.read();
        let base_fee = ctx.base_fee.max(self.config.min_fee_per_op);

        // Phase 1: Shared stateless structural validation
        // Mirrors stellar-core's commonValidPreSeqNum subset.
        henyey_tx::check_valid_pre_seq_num_with_config(
            &frame,
            ctx.protocol_version,
            ctx.ledger_flags,
            ctx.soroban_resource_limits.as_ref(),
        )
        .map_err(|e| e.to_tx_result_code())?;

        // Queue admission only: validate host function pairing.
        // stellar-core enforces this at queue admission but not tx-set checkValid.
        if frame.is_soroban() && !frame.validate_host_fn() {
            return Err(stellar_xdr::TransactionResultCode::TxSorobanInvalid);
        }

        // Build ledger context once for bounds and signature validation.
        let ledger_ctx = LedgerContext::new(
            ctx.ledger_seq,
            ctx.close_time,
            base_fee,
            ctx.base_reserve,
            ctx.protocol_version,
            self.config.network_id,
        );

        // Validate time/ledger bounds if enabled.
        // Parity: stellar-core TransactionQueue::tryAdd uses isTooEarly then isTooLate
        // with getUpperBoundCloseTimeOffset for the "too late" check.
        if self.config.validate_bounds {
            // Combined "too early" check: minTime OR minLedger
            if is_too_early(&frame, &ledger_ctx).is_err() {
                return Err(TxResultCode::TxTooEarly);
            }

            // For "too late" check: add upper bound offset to close time.
            // upperBound = expected_close_time * EXPECTED_CLOSE_TIME_MULT + drift
            // where drift = max(0, now - lcl_close_time).
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let drift = now.saturating_sub(ctx.close_time);
            let upper_offset =
            // .as_secs() floors to whole seconds — matches stellar-core's duration_cast<seconds>
                ctx.expected_close_time.as_secs() * EXPECTED_CLOSE_TIME_MULT + drift;
            let upper_close_time = ctx.close_time.saturating_add(upper_offset);
            let upper_ctx = LedgerContext::new(
                ctx.ledger_seq,
                upper_close_time,
                base_fee,
                ctx.base_reserve,
                ctx.protocol_version,
                self.config.network_id,
            );
            // Combined "too late" check: maxTime OR maxLedger
            if is_too_late(&frame, &upper_ctx).is_err() {
                return Err(TxResultCode::TxTooLate);
            }
        }

        // Validate signatures if enabled
        if self.config.validate_signatures {
            if validate_signatures(&frame, &ledger_ctx).is_err() {
                return Err(TxResultCode::TxBadAuth);
            }
        }

        // Validate preconditions (extra signers / min seq age+gap)
        if let Preconditions::V2(cond) = frame.preconditions() {
            if !cond.extra_signers.is_empty() {
                match extra_signers_satisfied(
                    envelope,
                    &self.config.network_id,
                    &cond.extra_signers,
                ) {
                    Ok(true) => {}
                    _ => return Err(TxResultCode::TxBadAuth),
                }
            }
        }

        // Per-op structural validation (isOpSupported + doCheckValid).
        // Runs AFTER signature validation to match stellar-core's apply-path
        // ordering where processSignatures precedes per-op checkValid.
        if !frame.is_soroban() {
            use henyey_tx::OperationTypeExt;
            let inner_source_id = frame.inner_source_account_id();
            for op in frame.operations().iter() {
                let op_type = henyey_tx::OperationType::from_body(&op.body);
                if henyey_tx::is_op_supported(&op_type, ctx.protocol_version, ctx.ledger_flags)
                    .is_err()
                {
                    return Err(stellar_xdr::TransactionResultCode::TxFailed);
                }
                let effective_source = match &op.source_account {
                    Some(muxed) => henyey_tx::muxed_to_account_id(muxed),
                    None => inner_source_id.clone(),
                };
                if henyey_tx::validate_classic_op_structure(
                    op,
                    ctx.protocol_version,
                    Some(&effective_source),
                )
                .is_err()
                {
                    return Err(stellar_xdr::TransactionResultCode::TxFailed);
                }
            }
        }

        Ok(())
    }

    /// Check that a Soroban transaction's declared resources don't exceed
    /// per-transaction network config limits.
    ///
    /// Parity: stellar-core `TransactionFrame::checkSorobanResources()`.
    fn check_soroban_resources(
        &self,
        env: &stellar_xdr::TransactionEnvelope,
    ) -> std::result::Result<(), String> {
        let ctx = self.validation_context.read();
        let Some(ref limits) = ctx.soroban_limits else {
            // No limits configured — skip check
            return Ok(());
        };

        let Some(data) = henyey_tx::envelope_utils::envelope_soroban_data(env) else {
            return Err("missing soroban transaction data".to_string());
        };

        let resources = &data.resources;

        if resources.instructions as u64 > limits.tx_max_instructions {
            return Err(format!(
                "instructions {} exceed limit {}",
                resources.instructions, limits.tx_max_instructions
            ));
        }

        if resources.disk_read_bytes as u64 > limits.tx_max_read_bytes {
            return Err(format!(
                "read bytes {} exceed limit {}",
                resources.disk_read_bytes, limits.tx_max_read_bytes
            ));
        }

        if resources.write_bytes as u64 > limits.tx_max_write_bytes {
            return Err(format!(
                "write bytes {} exceed limit {}",
                resources.write_bytes, limits.tx_max_write_bytes
            ));
        }

        let read_entries = resources.footprint.read_only.len() as u64;
        let write_entries = resources.footprint.read_write.len() as u64;

        if write_entries > limits.tx_max_write_ledger_entries {
            return Err(format!(
                "write entries {} exceed limit {}",
                write_entries, limits.tx_max_write_ledger_entries
            ));
        }

        if read_entries + write_entries > limits.tx_max_read_ledger_entries {
            return Err(format!(
                "read entries {} exceed limit {}",
                read_entries + write_entries,
                limits.tx_max_read_ledger_entries
            ));
        }

        let tx_size = henyey_tx::envelope_utils::envelope_tx_size_bytes(env) as u64;
        if tx_size > limits.tx_max_size_bytes {
            return Err(format!(
                "tx size {} exceeds limit {}",
                tx_size, limits.tx_max_size_bytes
            ));
        }

        Ok(())
    }

    /// Check per-account limit: one pending transaction per sequence-number source.
    ///
    /// Returns `Ok(None)` if no existing transaction, `Ok(Some(replaced))` if a
    /// fee-bump replacement is valid, or `Err(result)` for early rejection.
    fn check_account_limit(
        &self,
        queued: &QueuedTransaction,
        seq_source_key: &[u8],
        new_seq: i64,
        is_fee_bump: bool,
        candidate_is_soroban: bool,
    ) -> std::result::Result<Option<QueuedTransaction>, TxQueueResult> {
        let account_states = self.account_states.read();
        if let Some(state) = account_states.get(seq_source_key) {
            if let Some(ref current_tx) = state.transaction {
                if current_tx.hash == queued.hash {
                    return Err(TxQueueResult::Duplicate);
                }

                // Parity: stellar-core HerderImpl.cpp:627-642 rejects
                // submissions when the account has a pending tx of the
                // opposite type (classic vs soroban). This fires before
                // all queue-local checks (seq, fee-bump, RBF fee) so
                // that cross-type submissions always get TryAgainLater.
                let current_is_soroban =
                    henyey_tx::envelope_utils::is_soroban_envelope(&current_tx.envelope);
                if current_is_soroban != candidate_is_soroban {
                    return Err(TxQueueResult::TryAgainLater);
                }

                let current_seq = envelope_sequence_number(&current_tx.envelope);
                if new_seq < current_seq {
                    // Parity: stellar-core TransactionQueue::canAdd returns
                    // ADD_STATUS_ERROR with txBAD_SEQ when the new tx's seq is
                    // below the pending tx's seq for the same account.
                    return Err(TxQueueResult::Invalid(Some(
                        henyey_tx::TxResultCode::TxBadSeq,
                    )));
                }

                if !is_fee_bump {
                    return Err(TxQueueResult::TryAgainLater);
                }

                if new_seq != current_seq {
                    return Err(TxQueueResult::TryAgainLater);
                }

                if let Err(_min_fee) = can_replace_by_fee(&queued.fee_rate, &current_tx.fee_rate) {
                    return Err(TxQueueResult::FeeTooLow);
                }

                return Ok(Some(current_tx.clone()));
            }

            // Parity: authoritative recheck for fee-source-only entries.
            // stellar-core's sourceAccountPending also matches fee-source-only
            // entries in the opposite queue's mAccountStates.
            if candidate_is_soroban && state.classic_fee_tx_count > 0 {
                return Err(TxQueueResult::TryAgainLater);
            }
            if !candidate_is_soroban && state.soroban_fee_tx_count > 0 {
                return Err(TxQueueResult::TryAgainLater);
            }
        }
        Ok(None)
    }

    /// Build a `DexLimitingLaneConfig` from the queue's classic-lane settings.
    ///
    /// Returns `None` when neither `max_queue_classic_bytes` nor `max_queue_dex_ops`
    /// is configured (i.e. classic lane limits are disabled).
    fn build_classic_lane_config(&self) -> Option<DexLimitingLaneConfig> {
        if self.config.max_queue_classic_bytes.is_none() && self.config.max_queue_dex_ops.is_none()
        {
            return None;
        }
        let use_bytes = self.config.max_queue_classic_bytes.is_some();
        let ops_limit = i64::MAX;
        let generic_limit = if use_bytes {
            let bytes_limit = self.config.max_queue_classic_bytes.unwrap_or(u32::MAX) as i64;
            Resource::new(vec![ops_limit, bytes_limit])
        } else {
            Resource::new(vec![ops_limit])
        };
        let dex_limit = self.config.max_queue_dex_ops.map(|dex_ops| {
            if use_bytes {
                Resource::new(vec![dex_ops as i64, MAX_CLASSIC_BYTE_ALLOWANCE as i64])
            } else {
                Resource::new(vec![dex_ops as i64])
            }
        });
        Some(DexLimitingLaneConfig::new(generic_limit, dex_limit))
    }

    /// Record evicted transactions into the pending lists and update per-lane
    /// eviction fee thresholds.
    fn record_lane_evictions(
        &self,
        lane_config: &dyn SurgePricingLaneConfig,
        lane_fees_lock: &RwLock<Vec<Option<FeeRate>>>,
        evictions: Vec<(QueuedTransaction, bool)>,
        pending_evictions: &mut HashSet<Hash256>,
        pending_eviction_list: &mut Vec<QueuedTransaction>,
    ) {
        for (evicted, evicted_due_to_lane_limit) in evictions {
            if !pending_evictions.insert(evicted.hash) {
                continue;
            }
            let lane = lane_config.get_lane(&evicted.envelope);
            {
                let mut lane_fees = lane_fees_lock.write();
                if lane_fees.len() != lane_config.lane_limits().len() {
                    lane_fees.resize(lane_config.lane_limits().len(), None);
                }
                if evicted_due_to_lane_limit {
                    lane_fees[lane] = Some(evicted.fee_rate);
                } else {
                    lane_fees[GENERIC_LANE] = Some(evicted.fee_rate);
                }
            }
            pending_eviction_list.push(evicted);
        }
    }

    /// Check whether a transaction's fee is too low to beat the cached eviction
    /// thresholds for the given lane config.
    ///
    /// Returns `true` if the fee is too low and the transaction should be rejected.
    fn fee_below_lane_threshold(
        &self,
        lane_config: &dyn SurgePricingLaneConfig,
        lane_fees: &mut Vec<Option<FeeRate>>,
        envelope: &stellar_xdr::TransactionEnvelope,
        queued: &QueuedTransaction,
    ) -> bool {
        let lane = lane_config.get_lane(envelope);
        if lane_fees.len() != lane_config.lane_limits().len() {
            lane_fees.resize(lane_config.lane_limits().len(), None);
        }
        let global_fee = *self.eviction_thresholds.global_fees.read();
        let mut min_fee = min_inclusion_fee_to_beat(lane_fees[lane].as_ref(), &queued.fee_rate);
        min_fee = min_fee.max(min_inclusion_fee_to_beat(
            lane_fees[GENERIC_LANE].as_ref(),
            &queued.fee_rate,
        ));
        if self.effective_max_queue_ops().is_some() {
            min_fee = min_fee.max(min_inclusion_fee_to_beat(
                global_fee.as_ref(),
                &queued.fee_rate,
            ));
        }
        min_fee > 0
    }

    /// Check lane-based eviction fees and collect evictions for all applicable lanes.
    ///
    /// Returns the list of transactions to evict, or an early rejection result.
    fn check_and_collect_evictions(
        &self,
        store: &mut QueueStore,
        candidate: &EvictionCandidate,
        replaced_tx: Option<&QueuedTransaction>,
    ) -> std::result::Result<Vec<QueuedTransaction>, TxQueueResult> {
        // Phase 1: Check minimum inclusion fee for each lane (cheap, read-only)
        if !candidate.is_soroban {
            if let Some(lane_config) = self.build_classic_lane_config() {
                let mut lane_fees = self.eviction_thresholds.classic_lane_fees.write();
                if self.fee_below_lane_threshold(
                    &lane_config,
                    &mut lane_fees,
                    &candidate.queued.envelope,
                    candidate.queued,
                ) {
                    return Err(TxQueueResult::FeeTooLow);
                }
            }
        }

        if candidate.is_soroban {
            if let Some(limit) = self.effective_queue_soroban_resources() {
                let lane_config = SorobanGenericLaneConfig::new(limit);
                let mut lane_fees = self.eviction_thresholds.soroban_lane_fees.write();
                if self.fee_below_lane_threshold(
                    &lane_config,
                    &mut lane_fees,
                    &candidate.queued.envelope,
                    candidate.queued,
                ) {
                    return Err(TxQueueResult::FeeTooLow);
                }
            }
        }

        if self.effective_max_queue_ops().is_some() {
            let global_fee = *self.eviction_thresholds.global_fees.read();
            if min_inclusion_fee_to_beat(global_fee.as_ref(), &candidate.queued.fee_rate) > 0 {
                return Err(TxQueueResult::FeeTooLow);
            }
        }

        // Phase 2: Collect evictions using persistent queues (O(k) where k=evictions)
        let mut pending_evictions: HashSet<Hash256> = HashSet::new();
        let mut pending_eviction_list: Vec<QueuedTransaction> = Vec::new();

        if !candidate.is_soroban {
            // Parity: stellar-core TxQueueLimiter.cpp:133-136 asserts
            // oldTx->isSoroban() == newTx->isSoroban(). The cross-type
            // case is rejected in check_account_limit; assert here as
            // defense-in-depth.
            assert!(
                replaced_tx
                    .map(|tx| !henyey_tx::envelope_utils::is_soroban_envelope(&tx.envelope))
                    .unwrap_or(true),
                "cross-type replaced_tx in classic eviction pass"
            );
            if let Some(lane_config) = self.build_classic_lane_config() {
                store.ensure_classic_queue(lane_config.clone(), candidate.ledger_version);
                let exclusion = build_eviction_exclusion(
                    store.classic_eviction_queue.as_ref().unwrap(),
                    &store.by_hash,
                    replaced_tx,
                    &pending_evictions,
                    candidate.ledger_version,
                );
                let excl_ref = if exclusion.is_empty() {
                    None
                } else {
                    Some(&exclusion)
                };
                let Some(evictions) = store
                    .classic_eviction_queue
                    .as_ref()
                    .unwrap()
                    .can_fit_with_eviction(
                        candidate.queued,
                        None,
                        candidate.ledger_version,
                        excl_ref,
                    )
                else {
                    return Err(TxQueueResult::QueueFull);
                };
                self.record_lane_evictions(
                    &lane_config,
                    &self.eviction_thresholds.classic_lane_fees,
                    evictions,
                    &mut pending_evictions,
                    &mut pending_eviction_list,
                );
            }
        }

        if candidate.is_soroban {
            // Parity: stellar-core TxQueueLimiter.cpp:133-136 asserts
            // oldTx->isSoroban() == newTx->isSoroban().
            assert!(
                replaced_tx
                    .map(|tx| henyey_tx::envelope_utils::is_soroban_envelope(&tx.envelope))
                    .unwrap_or(true),
                "cross-type replaced_tx in soroban eviction pass"
            );
            if let Some(limit) = self.effective_queue_soroban_resources() {
                store.ensure_soroban_queue(limit.clone(), candidate.ledger_version);
                let exclusion = build_eviction_exclusion(
                    store.soroban_eviction_queue.as_ref().unwrap(),
                    &store.by_hash,
                    replaced_tx,
                    &pending_evictions,
                    candidate.ledger_version,
                );
                let excl_ref = if exclusion.is_empty() {
                    None
                } else {
                    Some(&exclusion)
                };
                let Some(evictions) = store
                    .soroban_eviction_queue
                    .as_ref()
                    .unwrap()
                    .can_fit_with_eviction(
                        candidate.queued,
                        None,
                        candidate.ledger_version,
                        excl_ref,
                    )
                else {
                    return Err(TxQueueResult::QueueFull);
                };
                let lane_config_for_record = SorobanGenericLaneConfig::new(limit);
                self.record_lane_evictions(
                    &lane_config_for_record,
                    &self.eviction_thresholds.soroban_lane_fees,
                    evictions,
                    &mut pending_evictions,
                    &mut pending_eviction_list,
                );
            }
        }

        if let Some(limit) = self.effective_max_queue_ops() {
            store.ensure_global_ops_queue(limit as i64, candidate.ledger_version);
            let exclusion = build_eviction_exclusion(
                store.global_ops_queue.as_ref().unwrap(),
                &store.by_hash,
                replaced_tx,
                &pending_evictions,
                candidate.ledger_version,
            );
            let excl_ref = if exclusion.is_empty() {
                None
            } else {
                Some(&exclusion)
            };
            let Some(evictions) = store
                .global_ops_queue
                .as_ref()
                .unwrap()
                .can_fit_with_eviction(candidate.queued, None, candidate.ledger_version, excl_ref)
            else {
                return Err(TxQueueResult::QueueFull);
            };
            for (evicted, _evicted_due_to_lane_limit) in evictions {
                if !pending_evictions.insert(evicted.hash) {
                    continue;
                }
                let mut global_fee = self.eviction_thresholds.global_fees.write();
                *global_fee = Some(evicted.fee_rate);
                pending_eviction_list.push(evicted);
            }
        }

        Ok(pending_eviction_list)
    }

    /// Try to add a transaction to the queue.
    pub fn try_add(&self, envelope: TransactionEnvelope) -> TxQueueResult {
        // Pre-compute values needed by both the early cross-type guard and
        // the later account-limit / admission logic, avoiding redundant
        // allocations on this hot path.
        let seq_source_key = account_key(&envelope);
        let candidate_is_soroban = henyey_tx::envelope_utils::is_soroban_envelope(&envelope);

        // Parity: stellar-core HerderImpl.cpp:627-645 checks whether the
        // source account already has a pending tx of the opposite type
        // (classic vs Soroban) BEFORE running any validation. Replicate that
        // precedence here so cross-type conflicts always surface as
        // TryAgainLater rather than whatever validation error would fire first.
        if let Some(result) =
            self.check_cross_type_conflict_with(&seq_source_key, candidate_is_soroban)
        {
            return result;
        }

        // Validate transaction before queueing
        if let Err(code) = self.validate_transaction(&envelope) {
            return TxQueueResult::Invalid(Some(code));
        }

        // Create queued transaction
        let queued = match QueuedTransaction::new(envelope) {
            Ok(q) => q,
            Err(e) => {
                // QueuedTransaction::new only fails on XDR-hash failure or
                // negative declared/inclusion fee — both indicate a malformed
                // envelope. Surface as txMALFORMED rather than a generic
                // internal error so clients don't retry or treat the tx as a
                // transient server fault.
                tracing::debug!(error = %e, "Rejecting malformed transaction");
                return TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxMalformed));
            }
        };

        // Check if already seen
        if self.seen.read().contains(&queued.hash) {
            return TxQueueResult::Duplicate;
        }

        // Check if banned
        if self.is_banned(&queued.hash) {
            return TxQueueResult::Banned;
        }

        // Check if filtered by operation type
        if self.is_filtered(&queued.envelope) {
            return TxQueueResult::Filtered;
        }

        // Check fee.
        //
        // Mirrors stellar-core TransactionFrame::commonValid (chargeFee path)
        // at TransactionFrame.cpp:1482-1487:
        //   getInclusionFee() < getMinInclusionFee(*this, header.current())
        //   ⇒ txINSUFFICIENT_FEE
        //
        // At admission time the Soroban-phase component base fee is unknown
        // (it's set by the nomination builder), so we use only the LCL base
        // fee — same as stellar-core which passes std::nullopt for baseFee.
        //
        // `min_fee_per_op` is preserved as a node-local floor on top of LCL.
        //
        // Regression: AUDIT-214 (#2103). Previously this gate compared
        // `total_fee / op_count` against `base_fee`, which let
        // resource-fee-heavy Soroban transactions (with `inclusion_fee == 0`)
        // through admission only to be rejected by the herder's own
        // `check_fee_map` after nomination.
        let lcl_base_fee = {
            let ctx = self.validation_context.read();
            ctx.base_fee.max(self.config.min_fee_per_op) as i64
        };
        // queued.op_count is computed by envelope_operation_count, the same
        // function `frame.resource_operation_count()` calls (returning
        // inner+1 for fee-bumps), so we don't need to construct a frame.
        let required_inclusion_fee =
            lcl_base_fee.saturating_mul(std::cmp::max(1i64, queued.op_count() as i64));
        if queued.inclusion_fee_i64() < required_inclusion_fee {
            return TxQueueResult::FeeTooLow;
        }

        let mut store = self.store.write();
        let ledger_version = self.validation_context.read().protocol_version;
        let queued_is_soroban = candidate_is_soroban;

        // Re-check ban after acquiring store.write() to close the TOCTOU
        // window with ban(). The early is_banned() check (above) is a
        // fast-path that avoids the store lock; this re-check ensures we
        // see any ban() that completed between the two checks.
        if self.is_banned(&queued.hash) {
            return TxQueueResult::Banned;
        }

        // Parity: check Soroban resource limits against network config
        if queued_is_soroban {
            if let Err(reason) = self.check_soroban_resources(&queued.envelope) {
                tracing::debug!(
                    hash = %queued.hash,
                    reason = %reason,
                    "Rejecting Soroban tx: resources exceed network config"
                );
                // Parity: stellar-core rejects resource-exceeding Soroban txs
                // with txSOROBAN_INVALID.
                return TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxSorobanInvalid));
            }
        }

        // Check for duplicate in queue
        if store.contains_key(&queued.hash) {
            return TxQueueResult::Duplicate;
        }

        // Per-account limit check: one transaction per account (sequence-number-source)
        // seq_source_key was pre-computed at the top of try_add.
        let new_seq = envelope_sequence_number(&queued.envelope);
        let is_fee_bump = is_fee_bump_envelope(&queued.envelope);
        let new_fee_source_key = fee_source_key(&queued.envelope);

        let replaced_tx = match self.check_account_limit(
            &queued,
            &seq_source_key,
            new_seq,
            is_fee_bump,
            queued_is_soroban,
        ) {
            Ok(replaced) => replaced,
            Err(result) => return result,
        };

        let candidate = EvictionCandidate {
            queued: &queued,
            is_soroban: queued_is_soroban,
            ledger_version,
        };

        let pending_eviction_list =
            match self.check_and_collect_evictions(&mut store, &candidate, replaced_tx.as_ref()) {
                Ok(evictions) => evictions,
                Err(result) => return result,
            };

        // Fee balance validation (pure check, no side effects — run before capacity eviction)
        if let Err(result) =
            self.validate_fee_balance(&queued, &new_fee_source_key, replaced_tx.as_ref())
        {
            return result;
        }

        // Check queue size (accounting for pending evictions) and evict if needed.
        if let Err(result) = self.ensure_queue_capacity(
            &mut store,
            pending_eviction_list.len(),
            &queued,
            ledger_version,
        ) {
            return result;
        }

        // Commit pending evictions now that all validation has passed.
        // This is deferred from check_and_collect_evictions to match stellar-core's
        // tryAdd which only calls evictTransactions after canAdd succeeds.
        // Parity: stellar-core TransactionQueue.cpp:733-739 bans each evicted
        // victim so it cannot be re-submitted immediately.
        for evicted in &pending_eviction_list {
            store.remove(&evicted.hash, ledger_version);
        }
        if !pending_eviction_list.is_empty() {
            // Lock order: account_states → banned → seen (canonical order
            // within the store scope — see TransactionQueue doc comment).
            let mut account_states = self.account_states.write();
            let mut banned = self.banned_transactions.write();
            let mut seen = self.seen.write();
            for evicted in &pending_eviction_list {
                seen.remove(&evicted.hash);
                Self::drop_transaction(&mut account_states, evicted);
            }
            // Ban evicted hashes so they cannot be re-submitted immediately.
            if let Some(newest) = banned.back_mut() {
                for evicted in &pending_eviction_list {
                    newest.insert(evicted.hash);
                }
            }
        }

        // Handle fee-bump replacement if applicable
        if let Some(ref old_tx) = replaced_tx {
            // Remove the old transaction from store
            store.remove(&old_tx.hash, ledger_version);

            // If the old tx has a different fee-source, release the fee from that account
            // Lock order: account_states → seen (canonical, within store scope).
            let old_fee_source_key = fee_source_key(&old_tx.envelope);
            if old_fee_source_key != new_fee_source_key {
                let mut account_states = self.account_states.write();
                if let Some(old_fee_state) = account_states.get_mut(&old_fee_source_key) {
                    old_fee_state.total_fees = old_fee_state
                        .total_fees
                        .saturating_sub(old_tx.total_fee as i64);
                    // Decrement fee type count for the old tx
                    let old_is_soroban =
                        henyey_tx::envelope_utils::is_soroban_envelope(&old_tx.envelope);
                    if old_is_soroban {
                        old_fee_state.soroban_fee_tx_count =
                            old_fee_state.soroban_fee_tx_count.saturating_sub(1);
                    } else {
                        old_fee_state.classic_fee_tx_count =
                            old_fee_state.classic_fee_tx_count.saturating_sub(1);
                    }
                    // Remove the account state if it's empty
                    if old_fee_state.is_empty() {
                        account_states.remove(&old_fee_source_key);
                    }
                }
            }
            self.seen.write().remove(&old_tx.hash);
        }

        // Add to queue
        let hash = queued.hash;
        let new_fee = queued.total_fee;

        // Update account_states
        {
            let mut account_states = self.account_states.write();

            // Update the sequence-source account state (stores the pending transaction)
            let seq_state = account_states.entry(seq_source_key.clone()).or_default();

            // If replacing, and same fee source as old tx, adjust the fee delta
            let fee_to_add = if let Some(ref old_tx) = replaced_tx {
                let old_fee_source_key = fee_source_key(&old_tx.envelope);
                if old_fee_source_key == new_fee_source_key {
                    // Same fee source - only add the difference
                    // Fee type count stays the same (same fee source, same type)
                    (new_fee as i64).saturating_sub(old_tx.total_fee as i64)
                } else {
                    // Different fee source - add full new fee
                    new_fee as i64
                }
            } else {
                // New transaction - add full fee
                new_fee as i64
            };

            seq_state.transaction = Some(queued.clone());

            // Update the fee-source account state (tracks total_fees and fee type counts)
            // Note: seq_source and fee_source may be the same account
            if seq_source_key == new_fee_source_key {
                // Same account - already have the entry
                seq_state.total_fees = seq_state.total_fees.saturating_add(fee_to_add);
                // Update fee type count for the new tx (only if not a same-source replacement)
                if replaced_tx.as_ref().map_or(true, |old| {
                    fee_source_key(&old.envelope) != new_fee_source_key
                }) {
                    if queued_is_soroban {
                        seq_state.soroban_fee_tx_count += 1;
                    } else {
                        seq_state.classic_fee_tx_count += 1;
                    }
                }
            } else {
                // Different accounts - update fee-source separately
                let fee_state = account_states.entry(new_fee_source_key).or_default();
                fee_state.total_fees = fee_state.total_fees.saturating_add(fee_to_add);
                // Update fee type count for the new tx (only if not a same-source replacement)
                if replaced_tx.as_ref().map_or(true, |old| {
                    fee_source_key(&old.envelope) != fee_source_key(&queued.envelope)
                }) {
                    if queued_is_soroban {
                        fee_state.soroban_fee_tx_count += 1;
                    } else {
                        fee_state.classic_fee_tx_count += 1;
                    }
                }
            }
        }

        store.insert(queued, ledger_version);
        self.seen.write().insert(hash);

        TxQueueResult::Added
    }

    /// Ensure queue has capacity, evicting lowest-fee or expired transactions if needed.
    ///
    /// Primary path uses the fee index for O(log n) eviction. Falls back to an
    /// expired-tx scan only when fee-based eviction fails (incoming tx has worse fee).
    fn ensure_queue_capacity(
        &self,
        store: &mut QueueStore,
        pending_eviction_count: usize,
        queued: &QueuedTransaction,
        ledger_version: u32,
    ) -> std::result::Result<(), TxQueueResult> {
        let effective_len = store.len().saturating_sub(pending_eviction_count);
        if effective_len < self.effective_max_size() {
            return Ok(());
        }

        // Prefer evicting an expired tx first — this is a "free" eviction that
        // doesn't displace any valid live transaction, matching pre-refactor behavior.
        let expired_hash = store
            .iter()
            .find(|(_, tx)| tx.is_expired(self.config.max_age_secs))
            .map(|(h, _)| *h);
        if let Some(hash) = expired_hash {
            let evicted = store.remove(&hash, ledger_version).unwrap();
            // Lock order: account_states → seen (canonical, within store scope).
            let mut account_states = self.account_states.write();
            Self::drop_transaction(&mut account_states, &evicted);
            self.seen.write().remove(&hash);
            return Ok(());
        }

        // O(log n): no expired txs available, try to evict the lowest-fee transaction
        if let Some(min_entry) = store.lowest_fee().cloned() {
            if queued.is_better_than_entry(&min_entry) {
                let evict_hash = min_entry.hash;
                let evicted = store.remove(&evict_hash, ledger_version).unwrap();
                // Lock order: account_states → seen (canonical, within store scope).
                let mut account_states = self.account_states.write();
                Self::drop_transaction(&mut account_states, &evicted);
                self.seen.write().remove(&evict_hash);
                return Ok(());
            }
        }

        Err(TxQueueResult::QueueFull)
    }

    /// Validate that the fee source has sufficient balance for the transaction.
    fn validate_fee_balance(
        &self,
        queued: &QueuedTransaction,
        new_fee_source_key: &Vec<u8>,
        replaced_tx: Option<&QueuedTransaction>,
    ) -> std::result::Result<(), TxQueueResult> {
        #[cfg(any(test, feature = "test-utils"))]
        let skip_fee = self
            .skip_fee_balance_check
            .load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(any(test, feature = "test-utils")))]
        let skip_fee = false;

        let Some(ref provider) = *self.fee_balance_provider.read() else {
            return Ok(());
        };
        if skip_fee {
            return Ok(());
        }

        let fee_source_id = account_id_from_fee_source_key(new_fee_source_key);

        let net_new_fee = if let Some(old_tx) = replaced_tx {
            let old_fee_source_key = fee_source_key(&old_tx.envelope);
            if old_fee_source_key == *new_fee_source_key {
                (queued.total_fee as i64).saturating_sub(old_tx.total_fee as i64)
            } else {
                queued.total_fee as i64
            }
        } else {
            queued.total_fee as i64
        };

        let current_total_fees = {
            let account_states = self.account_states.read();
            account_states
                .get(new_fee_source_key)
                .map(|s| s.total_fees)
                .unwrap_or(0)
        };

        match provider.get_available_balance(&fee_source_id) {
            Ok(Some(available)) => {
                if available.saturating_sub(net_new_fee) < current_total_fees {
                    return Err(TxQueueResult::Invalid(Some(
                        henyey_tx::TxResultCode::TxInsufficientBalance,
                    )));
                }
            }
            Ok(None) => {
                return Err(TxQueueResult::Invalid(Some(
                    henyey_tx::TxResultCode::TxNoAccount,
                )));
            }
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    ?fee_source_id,
                    "fee balance lookup failed during admission"
                );
                return Err(TxQueueResult::Invalid(None));
            }
        }

        Ok(())
    }

    /// Drop a queued transaction from account_states, releasing fees and
    /// cleaning up empty entries.
    ///
    /// Mirrors stellar-core's `dropTransaction()` + `releaseFeeMaybeEraseAccountState()`.
    /// The caller must have already removed the transaction from the store.
    fn drop_transaction(
        account_states: &mut HashMap<Vec<u8>, AccountState>,
        queued: &QueuedTransaction,
    ) {
        let seq_source = account_key(&queued.envelope);
        let fee_source = fee_source_key(&queued.envelope);
        let is_soroban = henyey_tx::envelope_utils::is_soroban_envelope(&queued.envelope);

        // Clear the pending transaction on the seq-source account.
        if let Some(state) = account_states.get_mut(&seq_source) {
            if state.transaction.as_ref().map(|t| &t.hash) == Some(&queued.hash) {
                state.transaction = None;
                state.age = 0;
            }
        }

        // Release fees and decrement fee type count on the fee-source account.
        if let Some(fee_state) = account_states.get_mut(&fee_source) {
            fee_state.total_fees = fee_state.total_fees.saturating_sub(queued.total_fee as i64);
            if is_soroban {
                fee_state.soroban_fee_tx_count = fee_state.soroban_fee_tx_count.saturating_sub(1);
            } else {
                fee_state.classic_fee_tx_count = fee_state.classic_fee_tx_count.saturating_sub(1);
            }
        }

        // Remove empty account state entries.
        if account_states
            .get(&seq_source)
            .map_or(false, |s| s.is_empty())
        {
            account_states.remove(&seq_source);
        }
        if seq_source != fee_source
            && account_states
                .get(&fee_source)
                .map_or(false, |s| s.is_empty())
        {
            account_states.remove(&fee_source);
        }
    }

    /// Get a transaction by hash.
    pub fn get(&self, hash: &Hash256) -> Option<QueuedTransaction> {
        self.store.read().get(hash).cloned()
    }

    /// Check if a transaction is in the queue.
    pub fn contains(&self, hash: &Hash256) -> bool {
        self.store.read().contains_key(hash)
    }

    /// Get the number of pending transactions.
    pub fn len(&self) -> usize {
        self.store.read().len()
    }

    /// Sample the oldest queued transactions at least `min_age` old.
    ///
    /// Returns `(hash, age_ms)` pairs, oldest first, capped at `cap`.
    /// Diagnostic helper for stranded-tx attribution (maxtps_tail): a queued
    /// tx older than a couple of ledgers indicates the flood path failed to
    /// deliver it to the rotating leaders.
    pub fn sample_aged_txs(&self, min_age: std::time::Duration, cap: usize) -> Vec<(Hash256, u64)> {
        let store = self.store.read();
        let now = Instant::now();
        let mut aged: Vec<(Hash256, u64)> = store
            .values()
            .filter_map(|tx| {
                let age = now.saturating_duration_since(tx.received_at());
                (age >= min_age).then(|| (tx.hash(), age.as_millis() as u64))
            })
            .collect();
        aged.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        aged.truncate(cap);
        aged
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.store.read().is_empty()
    }

    /// Estimate the heap footprint of this queue's owned collections (#3845).
    ///
    /// Reads capacities/lengths only — it never iterates entries (the
    /// banned-transactions deque is bounded by the ban depth, so summing its
    /// inner sets' capacities is a small constant). Each lock is taken and
    /// released independently, so no two queue locks are held at once and the
    /// documented `store → account_states → banned → seen` order cannot be
    /// violated. The shared `Arc<TransactionEnvelope>` payloads are excluded —
    /// only the inline `QueuedTransaction` struct is counted — to avoid
    /// double-counting.
    pub fn estimate_heap_bytes(&self) -> usize {
        use henyey_common::memory::{
            btreemap_heap_bytes, hashmap_heap_bytes, hashset_heap_bytes, vecdeque_heap_bytes,
        };

        // store: by_hash HashMap + fee_index BTreeSet (same-module field access).
        let store_bytes = {
            let store = self.store.read();
            hashmap_heap_bytes(
                store.by_hash.capacity(),
                std::mem::size_of::<Hash256>(),
                std::mem::size_of::<QueuedTransaction>(),
            ) + btreemap_heap_bytes(store.fee_index.len(), std::mem::size_of::<FeeEntry>(), 0)
        };

        // seen: HashSet<Hash256>.
        let seen_bytes = {
            let seen = self.seen.read();
            hashset_heap_bytes(seen.capacity(), std::mem::size_of::<Hash256>())
        };

        // banned_transactions: VecDeque<HashSet<Hash256>> (depth bounded by ban depth).
        let banned_bytes = {
            let banned = self.banned_transactions.read();
            let outer =
                vecdeque_heap_bytes(banned.capacity(), std::mem::size_of::<HashSet<Hash256>>());
            let inner: usize = banned
                .iter()
                .map(|s| hashset_heap_bytes(s.capacity(), std::mem::size_of::<Hash256>()))
                .sum();
            outer + inner
        };

        // account_states: HashMap<Vec<u8>, AccountState> — add an estimate for
        // the heap-allocated XDR-encoded AccountId keys.
        let account_bytes = {
            let states = self.account_states.read();
            const ACCOUNT_KEY_HEAP_BYTES: usize = 40;
            hashmap_heap_bytes(
                states.capacity(),
                std::mem::size_of::<Vec<u8>>(),
                std::mem::size_of::<AccountState>(),
            ) + states.len() * ACCOUNT_KEY_HEAP_BYTES
        };

        store_bytes + seen_bytes + banned_bytes + account_bytes
    }

    /// Reset all lane-based and global eviction fee thresholds.
    ///
    /// Called whenever the queue is rebuilt or transactions are evicted/shifted
    /// so that stale minimum-fee requirements are not carried forward.
    /// Invalidate all persistent eviction queues and cached fee thresholds.
    /// Regenerates the eviction seed, causing queues to be lazily rebuilt.
    /// Used by shift(), clear(), and reset_and_rebuild().
    fn invalidate_all_eviction_state(&self, store: &mut QueueStore) {
        store.regenerate_eviction_seed();
        self.eviction_thresholds.reset_all();
    }

    /// Invalidate Soroban-only eviction state: drops the soroban persistent
    /// queue and resets soroban cached thresholds.
    /// Used by update_soroban_resource_limits().
    fn invalidate_soroban_eviction_state(&self, store: &mut QueueStore) {
        store.soroban_eviction_queue = None;
        self.eviction_thresholds.reset_soroban();
    }

    /// Clear expired transactions.
    pub fn evict_expired(&self) {
        let mut store = self.store.write();
        let mut account_states = self.account_states.write();
        let max_age = self.config.max_age_secs;
        let ledger_version = self.validation_context.read().protocol_version;
        // Collect expired transactions, then remove them so account_states
        // are properly cleaned up (fee release + empty entry removal).
        let expired_hashes: Vec<Hash256> = store
            .iter()
            .filter(|(_, tx)| tx.is_expired(max_age))
            .map(|(hash, _)| *hash)
            .collect();
        let mut did_remove = false;
        for hash in &expired_hashes {
            if let Some(removed) = store.remove(hash, ledger_version) {
                Self::drop_transaction(&mut account_states, &removed);
                did_remove = true;
            }
        }
        if !expired_hashes.is_empty() {
            let mut seen = self.seen.write();
            for hash in &expired_hashes {
                seen.remove(hash);
            }
        }

        // Reset eviction thresholds after aging to avoid carrying stale
        // min-fee requirements. Only reset if something was actually removed —
        // if the queue didn't change, cached thresholds are still valid.
        if did_remove {
            self.eviction_thresholds.reset_all();
        }
    }

    /// Clear all transactions.
    pub fn clear(&self) {
        let mut store = self.store.write();
        store.clear_data();
        self.invalidate_all_eviction_state(&mut store);
        self.account_states.write().clear();
        // Don't clear seen - prevents replay
    }

    /// Clear the seen set (for testing or reset).
    pub fn clear_seen(&self) {
        self.seen.write().clear();
    }

    /// Ban a list of transactions by hash.
    ///
    /// Banned transactions cannot be added to the queue again for `ban_depth`
    /// ledgers. This should be called when transactions become invalid or
    /// are evicted due to age.
    ///
    /// # Arguments
    ///
    /// * `tx_hashes` - Hashes of transactions to ban
    pub fn ban(&self, tx_hashes: &[Hash256]) {
        if tx_hashes.is_empty() {
            return;
        }

        // Lock order: store → account_states → banned → seen (canonical).
        let mut store = self.store.write();
        let mut account_states = self.account_states.write();
        let mut banned = self.banned_transactions.write();
        let mut seen = self.seen.write();
        let ledger_version = self.validation_context.read().protocol_version;

        // Add to the newest (back) set
        if let Some(newest) = banned.back_mut() {
            for hash in tx_hashes {
                newest.insert(*hash);
            }
        }

        // Also remove from the queue if present, cleaning up account_states.
        // Mirrors stellar-core's ban() which calls dropTransaction().
        let mut did_remove = false;
        for hash in tx_hashes {
            if let Some(removed) = store.remove(hash, ledger_version) {
                Self::drop_transaction(&mut account_states, &removed);
                did_remove = true;
            }
            seen.remove(hash);
        }

        // Reset cached thresholds if a banned tx was removed from the queue —
        // it may have been the one that set the eviction threshold.
        if did_remove {
            self.eviction_thresholds.reset_all();
        }
    }

    /// Check if a transaction is banned.
    ///
    /// # Arguments
    ///
    /// * `hash` - Hash of the transaction to check
    ///
    /// # Returns
    ///
    /// `true` if the transaction is currently banned.
    pub fn is_banned(&self, hash: &Hash256) -> bool {
        let banned = self.banned_transactions.read();
        banned.iter().any(|set| set.contains(hash))
    }

    /// Check if a transaction contains any filtered operation types.
    ///
    /// Returns `true` if the transaction contains at least one operation
    /// whose type is in the `filtered_operation_types` set.
    ///
    /// # Arguments
    ///
    /// * `envelope` - The transaction envelope to check
    ///
    /// # Returns
    ///
    /// `true` if the transaction should be filtered out.
    pub fn is_filtered(&self, envelope: &TransactionEnvelope) -> bool {
        // Skip check if no types are filtered
        if self.config.filtered_operation_types.is_empty() {
            return false;
        }

        let ops = match envelope {
            TransactionEnvelope::TxV0(env) => &env.tx.operations,
            TransactionEnvelope::Tx(env) => &env.tx.operations,
            TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
                FeeBumpTransactionInnerTx::Tx(inner) => &inner.tx.operations,
            },
        };

        ops.iter().any(|op| {
            let op_type = op.body.discriminant();
            self.config.filtered_operation_types.contains(&op_type)
        })
    }

    /// Remove applied transactions from the queue and reset their source account ages.
    ///
    /// This should be called after transactions are applied in a ledger, before `shift()`.
    ///
    /// For each applied transaction:
    /// 1. Find the account state by sequence-number-source
    /// 2. If a queued tx exists with seq_num <= applied seq_num, drop it
    /// 3. Reset the account's age to 0
    /// 4. Release the fee from the fee-source account's total_fees
    /// 5. Ban the applied transaction hash (prevents re-submission)
    ///
    /// # Arguments
    ///
    /// * `applied_txs` - List of (envelope, sequence_number) pairs for applied transactions
    pub fn remove_applied(&self, applied_txs: &[(TransactionEnvelope, i64)]) {
        if applied_txs.is_empty() {
            return;
        }

        // Lock order: store → account_states → banned (canonical).
        let mut store = self.store.write();
        let mut account_states = self.account_states.write();
        let mut banned = self.banned_transactions.write();
        let ledger_version = self.validation_context.read().protocol_version;

        // Collect fee releases to apply after processing all transactions
        let mut fee_releases: Vec<(Vec<u8>, i64, bool)> = Vec::new(); // (key, fee, is_soroban)
        let mut accounts_to_cleanup: Vec<Vec<u8>> = Vec::new();
        let mut removed_hashes: Vec<Hash256> = Vec::new();

        for (envelope, applied_seq) in applied_txs {
            let frame = henyey_tx::TransactionFrame::from_owned_with_network(
                envelope.clone(),
                self.config.network_id,
            );

            // Get sequence-number-source (inner source for fee-bump)
            let seq_source_id = henyey_tx::muxed_to_account_id(&frame.inner_source_account());
            let seq_source_key = account_key_from_account_id(&seq_source_id);

            // Get fee-source
            let fee_source_id = henyey_tx::muxed_to_account_id(&frame.fee_source_account());
            let fee_source_key = account_key_from_account_id(&fee_source_id);

            // Process sequence-source account
            if let Some(state) = account_states.get_mut(&seq_source_key) {
                if let Some(ref queued_tx) = state.transaction {
                    // Drop if queued tx has seq <= applied seq
                    if queued_tx.sequence_number() <= *applied_seq {
                        // Remove from store
                        let removed_hash = queued_tx.hash;
                        // [maxtps_tail] interleaving diagnostic: a drop where
                        // the queued hash differs from the applied hash means
                        // a same-account duplicate raced (loadgen rebuild or
                        // flood echo) — cap the log to the first few per call.
                        let applied_hash = Hash256::hash_xdr(envelope);
                        if removed_hash != applied_hash && removed_hashes.len() < 3 {
                            tracing::info!(
                                target: "maxtps_tail",
                                queued_hash8 = %&removed_hash.to_hex()[..16],
                                applied_hash8 = %&applied_hash.to_hex()[..16],
                                queued_seq = queued_tx.sequence_number(),
                                applied_seq = *applied_seq,
                                "remove_applied dropped superseded duplicate"
                            );
                        }
                        store.remove(&removed_hash, ledger_version);
                        removed_hashes.push(removed_hash);

                        // Collect fee release info
                        let tx_fee = queued_tx.total_fee as i64;
                        let tx_fee_source_key = self::fee_source_key(&queued_tx.envelope);
                        let tx_is_soroban =
                            henyey_tx::envelope_utils::is_soroban_envelope(&queued_tx.envelope);
                        fee_releases.push((tx_fee_source_key, tx_fee, tx_is_soroban));

                        state.transaction = None;
                        state.age = 0;
                    }
                }
            }

            // Ban the applied tx hash
            let applied_hash = Hash256::hash_xdr(envelope);
            if let Some(newest) = banned.back_mut() {
                newest.insert(applied_hash);
            }

            // Track accounts for cleanup
            accounts_to_cleanup.push(seq_source_key);
            if fee_source_key != accounts_to_cleanup.last().cloned().unwrap_or_default() {
                accounts_to_cleanup.push(fee_source_key);
            }
        }

        // Apply fee releases
        for (fee_source_key, tx_fee, is_soroban) in fee_releases {
            if let Some(fee_state) = account_states.get_mut(&fee_source_key) {
                fee_state.total_fees = fee_state.total_fees.saturating_sub(tx_fee);
                if is_soroban {
                    fee_state.soroban_fee_tx_count =
                        fee_state.soroban_fee_tx_count.saturating_sub(1);
                } else {
                    fee_state.classic_fee_tx_count =
                        fee_state.classic_fee_tx_count.saturating_sub(1);
                }
            }
        }

        // Clean up empty account states
        for account_key in accounts_to_cleanup {
            if let Some(state) = account_states.get(&account_key) {
                if state.is_empty() {
                    account_states.remove(&account_key);
                }
            }
        }
        // Clean up seen set for removed transactions
        if !removed_hashes.is_empty() {
            let mut seen = self.seen.write();
            for hash in &removed_hashes {
                seen.remove(hash);
            }
        }

        // Reset cached thresholds if any txs were removed — they may have
        // been the ones that set eviction thresholds. In practice shift()
        // follows shortly and does full invalidation, but this makes the
        // invariant explicit.
        if !removed_hashes.is_empty() {
            self.eviction_thresholds.reset_all();
        }
    }

    /// Shift the queue after a ledger close.
    ///
    /// This should be called after `remove_applied()`. It:
    /// 1. Rotates the ban deque (unbans old transactions, makes room for new bans)
    /// 2. Increments age for all accounts with pending transactions
    /// 3. Auto-bans transactions that reach pending_depth age
    /// 4. Resets eviction thresholds for the new ledger
    ///
    /// # Returns
    ///
    /// A `ShiftResult` with details about unbanned and auto-banned transactions.
    pub fn shift(&self) -> ShiftResult {
        // Lock order: store → account_states → banned (canonical).
        let mut store = self.store.write();
        let mut account_states = self.account_states.write();
        let mut banned = self.banned_transactions.write();
        let ledger_version = self.validation_context.read().protocol_version;

        // Remove the oldest set (front) to unban those transactions
        let unbanned_count = banned.pop_front().map(|s| s.len()).unwrap_or(0);

        // Add a new empty set at the back for the next ledger
        banned.push_back(HashSet::new());

        let mut evicted_due_to_age = 0;
        let mut accounts_to_remove = Vec::new();
        // Collect fee releases to apply after iteration (to avoid borrow conflicts)
        let mut fee_releases: Vec<(Vec<u8>, u64, bool)> = Vec::new(); // (key, fee, is_soroban)
        let mut evicted_hashes: Vec<Hash256> = Vec::new();
        let mut reflooded_txs: Vec<TransactionEnvelope> = Vec::new();

        // Process account states: increment age, auto-ban stale transactions
        for (account_key, state) in account_states.iter_mut() {
            // Only increment age if there's a pending transaction
            if let Some(ref queued_tx) = state.transaction {
                state.age += 1;

                // Sustain fix: re-mark a pending tx for flooding when it has
                // aged 2 ledgers without applying. A tx is erased from the
                // flood queue the first time it is advertised; if that advert
                // was lost (peer outbound overflow, connection churn) the tx
                // is stranded on this node — it only applies when THIS node's
                // nomination candidate wins (~1/23 ledgers on the 23-node
                // maxtps topology, ≈2 minutes), by which time its account's
                // next submission collides with the pending tx
                // (TryAgainLater) and a PayPregenerated load-run insta-fails.
                // Re-marking is cheap and precise: the per-peer advert history
                // ensures only peers that never received the advert get it.
                // Fires at ages 2 and 3 (once per level). Age 2 is the proven
                // rescue point (~1.4k pushes/ledger at 2000 tx/s); age 3 is a
                // last-chance retry one ledger before the age-4 ban — the
                // 2026-07-04 traces show fresh txs starving in the equal-fee
                // flood queue for 3+ ledgers (sent_to=0), and the age-2-only
                // push rescued too late for a handful of txs per 2M-tx run,
                // wedging their accounts' seq chains. Age 1 must NOT push:
                // ~50% of txs sit one ledger at sustained max (normal
                // latency), and pushing them amplified flood traffic ~6x and
                // choked the network into mass age-outs (iter-15 regression).
                if state.age == 2 || state.age == 3 {
                    store.flood_queue.mark_for_flood(queued_tx, ledger_version);
                    // Also hand the full envelope to the caller for a DIRECT
                    // push to every peer: re-advertising alone cannot recover
                    // a tx whose original advert was recorded as sent but
                    // whose demand/response was lost — the per-peer advert
                    // history suppresses the re-advert. A pushed body
                    // recovers any dissemination hole within one ledger.
                    reflooded_txs.push((*queued_tx.envelope).clone());
                    metrics::counter!("stellar_herder_tx_reflooded_total").increment(1);
                }

                // Auto-ban at pending_depth
                if state.age >= self.pending_depth {
                    // Add to banned set
                    if let Some(newest) = banned.back_mut() {
                        newest.insert(queued_tx.hash);
                    }
                    // Rare terminal event (a handful per multi-million-tx load
                    // run) and the head of every wedged-account chain in
                    // pregenerated load: once banned here, nothing resubmits
                    // the tx and the account's later seqs can never apply.
                    // WARN with identifiers so cross-node forensics can trace
                    // the dissemination failure that let it age out.
                    tracing::warn!(
                        target: "maxtps_ban",
                        hash = %queued_tx.hash,
                        source_account_key8 = %account_key
                            .iter()
                            .take(8)
                            .fold(String::new(), |mut s, b| {
                                use std::fmt::Write;
                                let _ = write!(s, "{b:02x}");
                                s
                            }),
                        seq = envelope_sequence_number(&queued_tx.envelope),
                        age = state.age,
                        "auto-ban: pending tx aged out without inclusion"
                    );
                    // Remove from store and track for seen cleanup
                    store.remove(&queued_tx.hash, ledger_version);
                    evicted_hashes.push(queued_tx.hash);

                    // Track fee release for the fee-source account
                    let tx_fee_source_key = fee_source_key(&queued_tx.envelope);
                    let tx_is_soroban =
                        henyey_tx::envelope_utils::is_soroban_envelope(&queued_tx.envelope);
                    fee_releases.push((tx_fee_source_key, queued_tx.total_fee, tx_is_soroban));

                    evicted_due_to_age += 1;

                    state.transaction = None;

                    // Mark for removal if no fees tracked (will check again after fee release)
                    if state.total_fees == 0 {
                        accounts_to_remove.push(account_key.clone());
                    } else {
                        state.age = 0;
                    }
                }
            }
        }

        // Apply fee releases
        for (fee_source_key, tx_fee, is_soroban) in fee_releases {
            if let Some(fee_state) = account_states.get_mut(&fee_source_key) {
                fee_state.total_fees = fee_state.total_fees.saturating_sub(tx_fee as i64);
                if is_soroban {
                    fee_state.soroban_fee_tx_count =
                        fee_state.soroban_fee_tx_count.saturating_sub(1);
                } else {
                    fee_state.classic_fee_tx_count =
                        fee_state.classic_fee_tx_count.saturating_sub(1);
                }
                // Mark for removal if now empty
                if fee_state.is_empty() && !accounts_to_remove.contains(&fee_source_key) {
                    accounts_to_remove.push(fee_source_key);
                }
            }
        }

        // Remove empty account states
        for account_key in accounts_to_remove {
            account_states.remove(&account_key);
        }

        // Invalidate all eviction state (seed + queues + thresholds) for the
        // new ledger. Parity: stellar-core regenerates mBroadcastSeed in shift()
        // and calls resetBestFeeTxs() with the new seed.
        // HERDER_SPEC §12.4: mBroadcastSeed reseeding on shift() — covered here
        // by invalidate_all_eviction_state() which regenerates the seed.
        self.invalidate_all_eviction_state(&mut store);

        // Clean up seen set for evicted transactions
        if !evicted_hashes.is_empty() {
            let mut seen = self.seen.write();
            for hash in &evicted_hashes {
                seen.remove(hash);
            }
        }

        // Clear arbitrage flood damping state for the new ledger.
        // Parity: stellar-core clears mArbitrageFloodDamping in shift().
        self.arb_damper.lock().clear();

        ShiftResult {
            unbanned_count,
            evicted_due_to_age,
            reflooded_txs,
        }
    }

    /// Reset and rebuild the transaction queue after a protocol upgrade.
    ///
    /// Mirrors upstream `SorobanTransactionQueue::resetAndRebuild()`. This is
    /// called when a protocol upgrade changes Soroban resource limits. The
    /// queue is drained, account states and seen hashes are cleared, and all
    /// transactions are re-added via `try_add()` so that the new limits take
    /// effect. Banned transactions are preserved across the rebuild.
    ///
    /// Returns the number of transactions successfully re-added.
    pub fn reset_and_rebuild(&self) -> usize {
        tracing::info!("Resetting transaction queue due to upgrade");

        // Extract all current transactions before clearing state.
        let existing_txs: Vec<TransactionEnvelope> = {
            let store = self.store.read();
            store
                .values()
                .map(|qt| Arc::unwrap_or_clone(qt.envelope.clone()))
                .collect()
        };

        // Clear queue state but preserve bans (bans cannot be invalidated
        // by a protocol upgrade, matching upstream).
        {
            let mut store = self.store.write();
            store.clear_data();
            self.invalidate_all_eviction_state(&mut store);
        }
        {
            let mut seen = self.seen.write();
            seen.clear();
        }
        {
            let mut account_states = self.account_states.write();
            account_states.clear();
        }

        // Re-add all existing transactions. The surge pricing logic in
        // try_add() will handle sorting and evictions based on new limits.
        let mut re_added = 0;
        for tx in existing_txs {
            if self.try_add(tx) == TxQueueResult::Added {
                re_added += 1;
            }
        }

        tracing::info!(re_added, "Transaction queue rebuild complete");
        re_added
    }

    /// Get the total number of currently banned transactions.
    /// [maxtps_ban] Forensic lookup: the pending transaction (hash, seq, age)
    /// for `account_id`'s seq-source state, if any. Used by the loadgen's
    /// fatal-reject diagnostic to distinguish a genuinely wedged pending tx
    /// (old age) from a bookkeeping leak (on-ledger seq already caught up).
    pub fn account_pending_info(&self, account_id: &AccountId) -> Option<(Hash256, i64, u32)> {
        let key = account_key_from_account_id(account_id);
        let states = self.account_states.read();
        let state = states.get(&key)?;
        let tx = state.transaction.as_ref()?;
        Some((tx.hash, envelope_sequence_number(&tx.envelope), state.age))
    }

    /// Drop the account's pending transaction if it is stale: its sequence
    /// number is `<= on_ledger_seq`, i.e. the ledger has already consumed
    /// that sequence slot, so the pending entry can never apply again.
    ///
    /// Self-heal for #3719: ledger state (account seq) advances at apply,
    /// but `remove_applied`/`shift` run in a later `spawn_blocking` — a
    /// submission landing in that window (or racing a duplicate that applied
    /// via another node) sees a just-consumed pending entry and gets
    /// `TryAgainLater`. stellar-core cannot observe this state (its queue
    /// cleanup is synchronous with close on the main thread), so dropping the
    /// stale entry and re-admitting matches core-observable behavior.
    ///
    /// The staleness condition is re-checked under the store/account locks so
    /// a concurrent `remove_applied` cannot double-release fees. The hash is
    /// NOT banned here — if the tx applied, `remove_applied` bans it on its
    /// own schedule. Returns `true` if a stale entry was dropped.
    pub fn drop_stale_pending(&self, account_id: &AccountId, on_ledger_seq: i64) -> bool {
        let key = account_key_from_account_id(account_id);

        // Lock order: store → account_states (canonical; see remove_applied).
        let mut store = self.store.write();
        let mut account_states = self.account_states.write();
        let ledger_version = self.validation_context.read().protocol_version;

        let Some(state) = account_states.get_mut(&key) else {
            return false;
        };
        let Some(ref queued_tx) = state.transaction else {
            return false;
        };
        if queued_tx.sequence_number() > on_ledger_seq {
            return false;
        }

        let removed_hash = queued_tx.hash;
        let tx_fee = queued_tx.total_fee as i64;
        let tx_fee_source_key = self::fee_source_key(&queued_tx.envelope);
        let tx_is_soroban = henyey_tx::envelope_utils::is_soroban_envelope(&queued_tx.envelope);

        store.remove(&removed_hash, ledger_version);
        state.transaction = None;
        state.age = 0;
        if state.is_empty() {
            account_states.remove(&key);
        }

        // Release the reserved fee on the fee-source account (mirrors
        // remove_applied's fee-release bookkeeping).
        if let Some(fee_state) = account_states.get_mut(&tx_fee_source_key) {
            fee_state.total_fees = fee_state.total_fees.saturating_sub(tx_fee);
            if tx_is_soroban {
                fee_state.soroban_fee_tx_count = fee_state.soroban_fee_tx_count.saturating_sub(1);
            } else {
                fee_state.classic_fee_tx_count = fee_state.classic_fee_tx_count.saturating_sub(1);
            }
            if fee_state.is_empty() {
                account_states.remove(&tx_fee_source_key);
            }
        }

        drop(account_states);
        drop(store);

        self.seen.write().remove(&removed_hash);
        self.eviction_thresholds.reset_all();

        tracing::info!(
            target: "maxtps_tail",
            hash8 = %&removed_hash.to_hex()[..16],
            on_ledger_seq,
            "dropped stale pending tx (seq already consumed on-ledger, #3719)"
        );
        true
    }

    pub fn banned_count(&self) -> usize {
        let banned = self.banned_transactions.read();
        banned.iter().map(|s| s.len()).sum()
    }

    /// Get the number of banned transactions at each depth level.
    ///
    /// Index 0 is the oldest (about to be unbanned), index ban_depth-1 is newest.
    #[cfg(test)]
    pub fn banned_count_by_depth(&self) -> Vec<usize> {
        let banned = self.banned_transactions.read();
        banned.iter().map(|s| s.len()).collect()
    }

    pub fn pending_accounts(&self) -> Vec<AccountId> {
        let store = self.store.read();
        let mut accounts: HashSet<Vec<u8>> = HashSet::new();
        let mut out = Vec::new();
        for tx in store.values() {
            let account_id = account_id_from_envelope(&tx.envelope);
            let key = account_key_from_account_id(&account_id);
            if accounts.insert(key) {
                out.push(account_id);
            }
        }
        out
    }

    /// Visit queued transactions in fee-descending order with lane-aware budgeting.
    ///
    /// Mirrors stellar-core's `popTopTxs(allowGaps=false)` budget semantics via a
    /// fresh, operation-only flood limiter. The limiter traversal is destructive,
    /// but it drains only the temporary limiter, not the transaction queue.
    ///
    /// Budget-fit checks happen before invoking the visitor. Remaining budget is
    /// decremented only for [`BroadcastVisitResult::Processed`] candidates.
    ///
    /// Transactions dampened by arbitrage flood damping are collected during
    /// traversal and banned after `visit_top_txs` completes, mirroring
    /// stellar-core's `broadcastSome → ban(banningTxs)` pattern. Banned
    /// transactions are removed from the queue and recorded in the ban window
    /// so they are not reconsidered on subsequent flood periods.
    pub fn broadcast_with_visitor<F>(&self, budget: &mut BroadcastBudget, mut visitor: F)
    where
        F: FnMut(&BroadcastCandidate) -> BroadcastVisitResult,
    {
        let ops_budget =
            i64::try_from(budget.ops_remaining).expect("broadcast ops budget exceeds i64::MAX");
        let dex_ops_budget = budget.dex_ops_remaining.map(|budget| {
            i64::try_from(budget).expect("broadcast DEX ops budget exceeds i64::MAX")
        });

        let ledger_version = self.validation_context.read().protocol_version;

        // Pre-borrow fields needed by the visitor closure so the borrow checker
        // sees them as independent from `self.store`.
        let arb_damper = &self.arb_damper;
        let arb_tx_seen = &self.arb_tx_seen;
        let arb_tx_dropped = &self.arb_tx_dropped;

        let mut lane_resources_left = Vec::new();
        let mut banning_hashes: Vec<Hash256> = Vec::new();

        {
            let mut store = self.store.write();

            // Match the flood queue's lane count. If the queue has a DEX lane
            // but the caller didn't pass a DEX budget, use i64::MAX (uncapped).
            let mut custom_limits = vec![Resource::new(vec![ops_budget])];
            if store.flood_queue.num_lanes() > 1 {
                let dex_limit = dex_ops_budget.unwrap_or(i64::MAX);
                custom_limits.push(Resource::new(vec![dex_limit]));
            }

            // Destructively drain the persistent flood queue. Visited entries
            // are erased; entries beyond budget persist for the next tick.
            // Matches stellar-core's popTopTxs(false) on mTxsToFlood.
            store.flood_queue.visit_top_txs(
                |tx| {
                    // Arbitrage flood damping: check before visitor.
                    // Mirrors stellar-core broadcastTx → allowTxBroadcast.
                    let ops = henyey_tx::envelope_utils::envelope_operations(&tx.envelope);
                    let arb_result = arb_damper.lock().allow_tx_broadcast(ops);
                    match arb_result {
                        arb_flood_damping::ArbBroadcastResult::Dampened => {
                            arb_tx_seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            arb_tx_dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            banning_hashes.push(tx.hash);
                            return VisitTxResult::Skipped;
                        }
                        arb_flood_damping::ArbBroadcastResult::Allowed => {
                            arb_tx_seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        arb_flood_damping::ArbBroadcastResult::NotArb => {}
                    }

                    let candidate = BroadcastCandidate {
                        hash: tx.hash,
                        op_count: tx.op_count(),
                        is_dex: tx.is_dex,
                    };
                    visitor(&candidate).into()
                },
                &mut lane_resources_left,
                ledger_version,
                &custom_limits,
            );
        }

        budget.ops_remaining = lane_resources_left
            .first()
            .and_then(|resource| resource.try_get_val(ResourceType::Operations))
            .and_then(|ops| usize::try_from(ops).ok())
            .unwrap_or(0);
        if let Some(dex_remaining) = &mut budget.dex_ops_remaining {
            *dex_remaining = lane_resources_left
                .get(1)
                .and_then(|resource| resource.try_get_val(ResourceType::Operations))
                .and_then(|ops| usize::try_from(ops).ok())
                .unwrap_or(0);
        }

        // Ban dampened arbitrage transactions after traversal.
        // Mirrors stellar-core's broadcastSome → ban(banningTxs).
        self.ban(&banning_hashes);
    }

    /// Re-mark all queued transactions for flooding.
    ///
    /// Called after ledger-close invalidation completes (matching stellar-core's
    /// `resetBestFeeTxs()` + `rebroadcast()`). Ensures that:
    /// - Surviving transactions get re-advertised to peers on subsequent ticks
    /// - New peers receive the full mempool contents
    ///
    /// Note: unlike stellar-core's immediate `broadcast(false)` call, henyey
    /// defers the actual send to the next flood tick (~200ms). This is an
    /// intentional minor divergence affecting only propagation latency.
    pub fn rebroadcast(&self) {
        let mut store = self.store.write();
        let ledger_version = self.validation_context.read().protocol_version;
        let txs: Vec<QueuedTransaction> = store.values().cloned().collect();
        store
            .flood_queue
            .reset_and_repopulate(txs.iter(), ledger_version);
    }

    /// Return transaction hashes ordered by fee per op (desc) then received time (asc).
    pub fn ordered_hashes_by_fee(&self, limit: usize) -> Vec<Hash256> {
        let store = self.store.read();
        let mut entries: Vec<_> = store
            .values()
            .map(|tx| (tx.fee_per_op, tx.received_at, tx.hash))
            .collect();
        entries.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.to_hex().cmp(&b.2.to_hex()))
        });
        entries
            .into_iter()
            .take(limit)
            .map(|entry| entry.2)
            .collect()
    }

    /// Get statistics about the transaction queue.
    pub fn stats(&self) -> TxQueueStats {
        // Acquire locks in canonical order (account_states before seen) to avoid
        // inversion with mutation paths that hold account_states → banned → seen.
        // We only need seen.len() so snapshot it separately.
        let account_states = self.account_states.read();

        let mut pending_count = 0;
        let mut account_count = 0;
        let mut pending_txs_age = [0usize; 4];

        for state in account_states.values() {
            if state.transaction.is_some() {
                pending_count += 1;
                account_count += 1;
                let bucket = std::cmp::min(state.age as usize, 3);
                pending_txs_age[bucket] += 1;
            }
        }

        drop(account_states);

        TxQueueStats {
            pending_count,
            account_count,
            banned_count: self.banned_count(),
            seen_count: self.seen.read().len(),
            arb_tx_seen: self.arb_tx_seen.load(std::sync::atomic::Ordering::Relaxed),
            arb_tx_dropped: self
                .arb_tx_dropped
                .load(std::sync::atomic::Ordering::Relaxed),
            pending_txs_age,
        }
    }

    /// Get the number of arb txs seen during broadcast (for metrics export).
    pub fn arb_tx_seen_count(&self) -> u64 {
        self.arb_tx_seen.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Get the number of arb txs dampened during broadcast (for metrics export).
    pub fn arb_tx_dropped_count(&self) -> u64 {
        self.arb_tx_dropped
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A transaction selected for broadcast by [`TransactionQueue::broadcast_with_visitor`].
#[derive(Debug, Clone)]
pub struct BroadcastCandidate {
    /// Hash of the transaction.
    pub hash: Hash256,
    /// Number of operations in the transaction.
    pub op_count: u32,
    /// Whether this transaction contains DEX operations.
    pub is_dex: bool,
}

/// Result returned by a broadcast visitor closure.
///
/// Unlike [`VisitTxResult`], this enum has no `Rejected` variant — budget-fit
/// checks happen before the visitor is invoked, so the visitor only decides
/// whether the candidate is useful (Processed) or redundant (Skipped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastVisitResult {
    /// The candidate was accepted (e.g., at least one peer needs it).
    /// Budget is consumed for this candidate.
    Processed,
    /// The candidate was skipped (e.g., all peers already have it).
    /// Budget is NOT consumed.
    Skipped,
}

impl From<BroadcastVisitResult> for VisitTxResult {
    fn from(value: BroadcastVisitResult) -> Self {
        match value {
            BroadcastVisitResult::Processed => VisitTxResult::Processed,
            BroadcastVisitResult::Skipped => VisitTxResult::Skipped,
        }
    }
}

/// Mutable budget state for [`TransactionQueue::broadcast_with_visitor`].
///
/// After traversal, `ops_remaining` and `dex_ops_remaining` reflect the
/// unconsumed budget, suitable for carry-over to the next flood period.
#[derive(Debug, Clone)]
pub struct BroadcastBudget {
    /// Remaining generic ops budget. Decremented only for `Processed` candidates.
    pub ops_remaining: usize,
    /// Remaining DEX ops budget. `None` means DEX flooding is uncapped.
    /// Decremented only for `Processed` DEX candidates.
    pub dex_ops_remaining: Option<usize>,
}

/// Statistics about the transaction queue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TxQueueStats {
    /// Number of pending transactions.
    pub pending_count: usize,
    /// Number of accounts with pending transactions.
    pub account_count: usize,
    /// Number of currently banned transactions.
    pub banned_count: usize,
    /// Number of seen (deduplicated) transaction hashes.
    pub seen_count: usize,
    /// Total arbitrage transactions evaluated for broadcast (monotonic).
    pub arb_tx_seen: u64,
    /// Total arbitrage transactions dropped by damping (monotonic).
    pub arb_tx_dropped: u64,
    /// Pending transaction count bucketed by age (slot age 0, 1, 2, 3+).
    /// Matches stellar-core's `herder.pending.txs.age{0,1,2,3}` gauges.
    pub pending_txs_age: [usize; 4],
}

fn extra_signers_satisfied(
    envelope: &TransactionEnvelope,
    network_id: &NetworkId,
    extra_signers: &[SignerKey],
) -> std::result::Result<bool, &'static str> {
    let (tx_hash, signatures) = precondition_hash_and_signatures(envelope, network_id)?;

    Ok(extra_signers.iter().all(|signer| match signer {
        SignerKey::Ed25519(key) => has_ed25519_signature(&tx_hash, signatures, &key.0),
        SignerKey::PreAuthTx(key) => key.0 == tx_hash.0,
        SignerKey::HashX(key) => has_hashx_signature(signatures, key),
        SignerKey::Ed25519SignedPayload(payload) => {
            has_signed_payload_signature(&tx_hash, signatures, payload)
        }
    }))
}

fn precondition_hash_and_signatures<'a>(
    envelope: &'a TransactionEnvelope,
    network_id: &NetworkId,
) -> std::result::Result<(Hash256, &'a [DecoratedSignature]), &'static str> {
    match envelope {
        TransactionEnvelope::TxV0(env) => {
            let frame =
                henyey_tx::TransactionFrame::from_owned_with_network(envelope.clone(), *network_id);
            let hash = frame.hash(network_id).map_err(|_| "tx hash error")?;
            Ok((hash, env.signatures.as_slice()))
        }
        TransactionEnvelope::Tx(env) => {
            let frame =
                henyey_tx::TransactionFrame::from_owned_with_network(envelope.clone(), *network_id);
            let hash = frame.hash(network_id).map_err(|_| "tx hash error")?;
            Ok((hash, env.signatures.as_slice()))
        }
        TransactionEnvelope::TxFeeBump(env) => {
            let inner_env = match &env.tx.inner_tx {
                FeeBumpTransactionInnerTx::Tx(inner) => inner.clone(),
            };
            let inner_frame = henyey_tx::TransactionFrame::from_owned_with_network(
                TransactionEnvelope::Tx(inner_env),
                *network_id,
            );
            let hash = inner_frame
                .hash(network_id)
                .map_err(|_| "inner tx hash error")?;
            let signatures = match &env.tx.inner_tx {
                FeeBumpTransactionInnerTx::Tx(inner) => inner.signatures.as_slice(),
            };
            Ok((hash, signatures))
        }
    }
}

fn has_ed25519_signature(
    tx_hash: &Hash256,
    signatures: &[DecoratedSignature],
    key_bytes: &[u8; 32],
) -> bool {
    signatures
        .iter()
        .any(|sig| henyey_tx::verify_signature_with_raw_key(tx_hash, sig, key_bytes))
}

fn has_hashx_signature(signatures: &[DecoratedSignature], key: &stellar_xdr::Uint256) -> bool {
    signatures.iter().any(|sig| {
        if sig.signature.0.len() != 32 {
            return false;
        }
        let expected_hint = [key.0[28], key.0[29], key.0[30], key.0[31]];
        if sig.hint.0 != expected_hint {
            return false;
        }
        let hash = Hash256::hash(&sig.signature.0);
        hash.0 == key.0
    })
}

fn has_signed_payload_signature(
    _tx_hash: &Hash256,
    signatures: &[DecoratedSignature],
    payload: &stellar_xdr::SignerKeyEd25519SignedPayload,
) -> bool {
    signatures
        .iter()
        .any(|sig| henyey_crypto::verify_ed25519_signed_payload(sig, payload))
}

impl Default for TransactionQueue {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_common::NetworkId;
    use henyey_common::{Resource, ResourceType, NUM_SOROBAN_TX_RESOURCES};
    use henyey_crypto::{sign_hash, SecretKey};
    use stellar_xdr::{
        AccountId, AlphaNum4, Asset, AssetCode4, ContractExecutable, ContractIdPreimage,
        ContractIdPreimageFromAddress, CreateAccountOp, CreateContractArgs, DecoratedSignature,
        Duration, FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
        FeeBumpTransactionInnerTx, Hash, HostFunction, InvokeContractArgs, InvokeHostFunctionOp,
        LedgerFootprint, ManageSellOfferOp, Memo, MuxedAccount, MuxedAccountMed25519, Operation,
        OperationBody, PaymentOp, Preconditions, PreconditionsV2, Price, PublicKey, ScAddress,
        ScSymbol, ScVal, SequenceNumber, Signature as XdrSignature, SignatureHint, SignerKey,
        SorobanResources, SorobanTransactionData, SorobanTransactionDataExt, StringM, Transaction,
        TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256, VecM,
    };

    fn make_test_envelope(fee: u32, ops: usize) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));

        let operations: Vec<Operation> = (0..ops)
            .map(|_| Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    // Use destination [255; 32] so it differs from any test source
                    destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([255u8; 32]))),
                    starting_balance: 1000000000,
                }),
            })
            .collect();

        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn make_soroban_envelope(fee: u32) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([9u8; 32]));
        let function_name = ScSymbol(StringM::<32>::try_from("test".to_string()).expect("symbol"));
        let host_function = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address: ScAddress::default(),
            function_name,
            args: VecM::<ScVal>::default(),
        });
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function,
                auth: VecM::default(),
            }),
        };

        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn make_soroban_envelope_with_resources(fee: u32, instructions: u32) -> TransactionEnvelope {
        let mut envelope = make_soroban_envelope(fee);
        if let TransactionEnvelope::Tx(env) = &mut envelope {
            let resources = SorobanResources {
                footprint: LedgerFootprint {
                    read_only: VecM::default(),
                    read_write: VecM::default(),
                },
                instructions,
                disk_read_bytes: 0,
                write_bytes: 0,
            };
            env.tx.ext = TransactionExt::V1(SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources,
                resource_fee: 0,
            });
        }
        envelope
    }

    fn make_soroban_envelope_with_resource_fee(
        fee: u32,
        resource_fee: i64,
        instructions: u32,
    ) -> TransactionEnvelope {
        let mut envelope = make_soroban_envelope_with_resources(fee, instructions);
        if let TransactionEnvelope::Tx(env) = &mut envelope {
            if let TransactionExt::V1(data) = &mut env.tx.ext {
                data.resource_fee = resource_fee;
            }
        }
        envelope
    }

    fn make_dex_envelope(fee: u32) -> TransactionEnvelope {
        make_dex_envelope_with_ops(fee, 1)
    }

    fn make_dex_envelope_with_ops(fee: u32, ops: usize) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([10u8; 32]));
        let selling = Asset::Native;
        let buying = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USDC"),
            issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([11u8; 32]))),
        });
        let operations: Vec<Operation> = (0..ops)
            .map(|_| Operation {
                source_account: None,
                body: OperationBody::ManageSellOffer(ManageSellOfferOp {
                    selling: selling.clone(),
                    buying: buying.clone(),
                    amount: 1,
                    price: Price { n: 1, d: 1 },
                    offer_id: 0,
                }),
            })
            .collect();

        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn sign_envelope(
        envelope: &TransactionEnvelope,
        secret: &SecretKey,
        network_id: &NetworkId,
    ) -> DecoratedSignature {
        let frame =
            henyey_tx::TransactionFrame::from_owned_with_network(envelope.clone(), *network_id);
        let hash = frame.hash(network_id).expect("tx hash");
        let signature = sign_hash(secret, &hash);

        let public_key = secret.public_key();
        let pk_bytes = public_key.as_bytes();
        let hint = SignatureHint([pk_bytes[28], pk_bytes[29], pk_bytes[30], pk_bytes[31]]);

        DecoratedSignature {
            hint,
            signature: XdrSignature(signature.0.to_vec().try_into().unwrap()),
        }
    }

    fn envelope_fee(envelope: &TransactionEnvelope) -> u64 {
        crate::tx_set_utils::envelope_fee(envelope).as_i64() as u64
    }

    fn envelope_seq(envelope: &TransactionEnvelope) -> i64 {
        match envelope {
            TransactionEnvelope::TxV0(tx) => tx.tx.seq_num.0,
            TransactionEnvelope::Tx(tx) => tx.tx.seq_num.0,
            TransactionEnvelope::TxFeeBump(tx) => match &tx.tx.inner_tx {
                stellar_xdr::FeeBumpTransactionInnerTx::Tx(inner) => inner.tx.seq_num.0,
            },
        }
    }

    fn envelope_size(envelope: &TransactionEnvelope) -> usize {
        envelope
            .to_xdr(stellar_xdr::Limits::none())
            .map(|bytes| bytes.len())
            .unwrap_or(0)
    }

    fn full_hash(envelope: &TransactionEnvelope) -> Hash256 {
        Hash256::hash_xdr(envelope)
    }

    fn set_source(envelope: &mut TransactionEnvelope, seed: u8) {
        let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
        match envelope {
            TransactionEnvelope::TxV0(env) => {
                env.tx.source_account_ed25519 = Uint256([seed; 32]);
            }
            TransactionEnvelope::Tx(env) => {
                env.tx.source_account = source;
            }
            TransactionEnvelope::TxFeeBump(env) => match &mut env.tx.inner_tx {
                stellar_xdr::FeeBumpTransactionInnerTx::Tx(inner) => {
                    inner.tx.source_account = source;
                }
            },
        }
    }

    #[test]
    fn test_add_transaction() {
        let queue = TransactionQueue::with_defaults();

        let tx = make_test_envelope(200, 1);
        let result = queue.try_add(tx);
        assert_eq!(result, TxQueueResult::Added);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_duplicate_detection() {
        let queue = TransactionQueue::with_defaults();

        let tx = make_test_envelope(200, 1);
        queue.try_add(tx.clone());
        let result = queue.try_add(tx);
        assert_eq!(result, TxQueueResult::Duplicate);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_ban_mechanism() {
        let queue = TransactionQueue::with_ban_depth(TxQueueConfig::default(), 3);

        // Create two transactions
        let tx1 = make_test_envelope(200, 1);
        let hash1 = Hash256::hash_xdr(&tx1);
        let mut tx2 = make_test_envelope(200, 1);
        set_source(&mut tx2, 2);
        let hash2 = Hash256::hash_xdr(&tx2);

        // Add tx1 to the queue
        assert_eq!(queue.try_add(tx1.clone()), TxQueueResult::Added);
        assert_eq!(queue.len(), 1);

        // Ban tx1 (which is in queue) and tx2 (which is not)
        queue.ban(&[hash1, hash2]);
        assert!(queue.is_banned(&hash1));
        assert!(queue.is_banned(&hash2));
        assert_eq!(queue.len(), 0); // tx1 should be removed from queue
        assert_eq!(queue.banned_count(), 2);

        // Try to add tx2 - should fail as banned (not in seen set)
        assert_eq!(queue.try_add(tx2.clone()), TxQueueResult::Banned);

        // tx1 was seen (added before ban), but ban() now clears seen.
        // So tx1 is rejected as Banned, not Duplicate.
        assert_eq!(queue.try_add(tx1.clone()), TxQueueResult::Banned);

        // Verify ban depth tracking
        let counts = queue.banned_count_by_depth();
        assert_eq!(counts.len(), 3);
        assert_eq!(counts[2], 2); // Newest set has both bans
        assert_eq!(counts[0], 0);
        assert_eq!(counts[1], 0);
    }

    #[test]
    fn test_ban_shift_unban() {
        let queue = TransactionQueue::with_ban_depth(TxQueueConfig::default(), 3);

        let tx = make_test_envelope(200, 1);
        let hash = Hash256::hash_xdr(&tx);
        queue.ban(&[hash]);
        assert!(queue.is_banned(&hash));

        // After 3 shifts, the ban should be removed
        queue.shift(); // ledger 1
        assert!(queue.is_banned(&hash));
        queue.shift(); // ledger 2
        assert!(queue.is_banned(&hash));
        let shift_result = queue.shift(); // ledger 3 - oldest set removed
        assert_eq!(shift_result.unbanned_count, 1);
        assert!(!queue.is_banned(&hash)); // Now unbanned

        // Should be able to add again
        assert_eq!(queue.try_add(tx), TxQueueResult::Added);
    }

    #[test]
    fn test_multiple_bans_across_ledgers() {
        let queue = TransactionQueue::with_ban_depth(TxQueueConfig::default(), 3);

        // Ban tx1 in ledger 1
        let mut tx1 = make_test_envelope(200, 1);
        set_source(&mut tx1, 1);
        let hash1 = Hash256::hash_xdr(&tx1);
        queue.ban(&[hash1]);

        queue.shift(); // ledger 2

        // Ban tx2 in ledger 2
        let mut tx2 = make_test_envelope(200, 1);
        set_source(&mut tx2, 2);
        let hash2 = Hash256::hash_xdr(&tx2);
        queue.ban(&[hash2]);

        queue.shift(); // ledger 3

        // Ban tx3 in ledger 3
        let mut tx3 = make_test_envelope(200, 1);
        set_source(&mut tx3, 3);
        let hash3 = Hash256::hash_xdr(&tx3);
        queue.ban(&[hash3]);

        // All should be banned
        assert!(queue.is_banned(&hash1));
        assert!(queue.is_banned(&hash2));
        assert!(queue.is_banned(&hash3));

        // After shift, tx1 should be unbanned
        queue.shift(); // ledger 4
        assert!(!queue.is_banned(&hash1));
        assert!(queue.is_banned(&hash2));
        assert!(queue.is_banned(&hash3));

        // After another shift, tx2 should be unbanned
        queue.shift(); // ledger 5
        assert!(!queue.is_banned(&hash2));
        assert!(queue.is_banned(&hash3));
    }

    #[test]
    fn test_fee_ordering() {
        let queue = TransactionQueue::with_defaults();

        // Add transactions with different fees
        let mut tx_low = make_test_envelope(100, 1);
        let mut tx_high = make_test_envelope(300, 1);
        let mut tx_mid = make_test_envelope(200, 1);
        set_source(&mut tx_low, 1);
        set_source(&mut tx_high, 2);
        set_source(&mut tx_mid, 3);
        assert_eq!(queue.try_add(tx_low), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_high), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);
        queue.try_add(tx_mid);

        let set = queue.get_transaction_set(Hash256::ZERO, 10);
        assert_eq!(set.len(), 3);

        let mut fees: Vec<u64> = set.iter_transactions().map(envelope_fee).collect();
        fees.sort_by(|a, b| b.cmp(a));
        assert_eq!(fees, vec![300, 200, 100]);
    }

    #[test]
    fn test_tie_breaker_is_deterministic() {
        let queue = TransactionQueue::with_defaults();
        let network_id = NetworkId::testnet();

        let mut tx_a = make_test_envelope(200, 1);
        let mut tx_b = make_test_envelope(200, 1);
        set_source(&mut tx_a, 4);
        set_source(&mut tx_b, 5);
        let hash_a = henyey_tx::TransactionFrame::from_owned_with_network(tx_a.clone(), network_id)
            .hash(&network_id)
            .expect("hash tx_a");
        let hash_b = henyey_tx::TransactionFrame::from_owned_with_network(tx_b.clone(), network_id)
            .hash(&network_id)
            .expect("hash tx_b");

        queue.try_add(tx_a);
        queue.try_add(tx_b);

        let set = queue.get_transaction_set(Hash256::ZERO, 10);
        assert_eq!(set.len(), 2);

        let expected = if hash_a.0 >= hash_b.0 {
            vec![hash_a, hash_b]
        } else {
            vec![hash_b, hash_a]
        };
        let got: Vec<Hash256> = set
            .iter_transactions()
            .map(|tx| {
                henyey_tx::TransactionFrame::from_owned_with_network(tx.clone(), network_id)
                    .hash(&network_id)
                    .expect("hash tx")
            })
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn test_sequence_gap_stops_layer() {
        let queue = TransactionQueue::with_defaults();

        let mut tx_a = make_test_envelope(200, 1);
        let mut tx_b = make_test_envelope(200, 1);
        if let TransactionEnvelope::Tx(env) = &mut tx_a {
            env.tx.seq_num = SequenceNumber(1);
        }
        if let TransactionEnvelope::Tx(env) = &mut tx_b {
            env.tx.seq_num = SequenceNumber(3);
        }

        queue.try_add(tx_a);
        queue.try_add(tx_b);

        let set = queue.get_transaction_set(Hash256::ZERO, 10);
        assert_eq!(set.len(), 1);
        assert_eq!(envelope_seq(&set.as_legacy_transactions().unwrap()[0]), 1);
    }

    #[test]
    fn test_sequence_order_preserved() {
        // With one-tx-per-account limit, each transaction needs a different source account
        let queue = TransactionQueue::with_defaults();

        let mut tx_a = make_test_envelope(200, 1);
        let mut tx_b = make_test_envelope(200, 1);
        set_source(&mut tx_a, 1);
        set_source(&mut tx_b, 2); // Different account
        if let TransactionEnvelope::Tx(env) = &mut tx_a {
            env.tx.seq_num = SequenceNumber(1);
        }
        if let TransactionEnvelope::Tx(env) = &mut tx_b {
            env.tx.seq_num = SequenceNumber(2);
        }

        queue.try_add(tx_a);
        queue.try_add(tx_b);

        let set = queue.get_transaction_set(Hash256::ZERO, 10);
        let mut seqs: Vec<i64> = set.iter_transactions().map(envelope_seq).collect();
        seqs.sort();
        assert_eq!(seqs, vec![1, 2]);
    }

    #[test]
    fn test_sequence_blocks_classic_after_soroban() {
        // With one-tx-per-account limit, only one tx per account can be added.
        // Use different accounts to test that both classic and soroban can coexist.
        let queue = TransactionQueue::with_defaults();

        let mut classic = make_test_envelope(250, 1);
        let mut soroban = make_soroban_envelope(200);
        set_source(&mut classic, 7);
        set_source(&mut soroban, 8); // Different account
        if let TransactionEnvelope::Tx(env) = &mut classic {
            env.tx.seq_num = SequenceNumber(1);
        }
        if let TransactionEnvelope::Tx(env) = &mut soroban {
            env.tx.seq_num = SequenceNumber(2);
        }

        assert_eq!(queue.try_add(classic), TxQueueResult::Added);
        assert_eq!(queue.try_add(soroban), TxQueueResult::Added);

        let set = queue.get_transaction_set(Hash256::ZERO, 10);
        let mut seqs: Vec<i64> = set.iter_transactions().map(envelope_seq).collect();
        seqs.sort();
        assert_eq!(seqs, vec![1, 2]);
    }

    #[test]
    fn test_sequence_allows_soroban_suffix() {
        // With one-tx-per-account limit, use different accounts for each transaction
        let queue = TransactionQueue::with_defaults();

        let mut classic = make_test_envelope(200, 1);
        let mut soroban_a = make_soroban_envelope(200);
        let mut soroban_b = make_soroban_envelope(200);
        set_source(&mut classic, 7);
        set_source(&mut soroban_a, 8); // Different account
        set_source(&mut soroban_b, 9); // Different account
        if let TransactionEnvelope::Tx(env) = &mut classic {
            env.tx.seq_num = SequenceNumber(1);
        }
        if let TransactionEnvelope::Tx(env) = &mut soroban_a {
            env.tx.seq_num = SequenceNumber(2);
        }
        if let TransactionEnvelope::Tx(env) = &mut soroban_b {
            env.tx.seq_num = SequenceNumber(3);
        }

        assert_eq!(queue.try_add(classic), TxQueueResult::Added);
        assert_eq!(queue.try_add(soroban_a), TxQueueResult::Added);
        assert_eq!(queue.try_add(soroban_b), TxQueueResult::Added);

        let set = queue.get_transaction_set(Hash256::ZERO, 10);
        let mut seqs: Vec<i64> = set.iter_transactions().map(envelope_seq).collect();
        seqs.sort();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn test_sequence_respects_starting_seq() {
        // With one-tx-per-account limit, use different accounts
        let queue = TransactionQueue::with_defaults();

        let mut tx_a = make_test_envelope(200, 1);
        let mut tx_b = make_test_envelope(200, 1);
        set_source(&mut tx_a, 1);
        set_source(&mut tx_b, 2); // Different account
        if let TransactionEnvelope::Tx(env) = &mut tx_a {
            env.tx.seq_num = SequenceNumber(5);
        }
        if let TransactionEnvelope::Tx(env) = &mut tx_b {
            env.tx.seq_num = SequenceNumber(6);
        }

        queue.try_add(tx_a.clone());
        queue.try_add(tx_b);

        // Set starting sequence for account 1 to 5, so tx_a (seq 5) should be filtered out
        let account_id = account_id_from_envelope(&tx_a);
        let mut starting = std::collections::HashMap::new();
        starting.insert(account_key_from_account_id(&account_id), 5);

        let set = queue.get_transaction_set_with_starting_seq(Hash256::ZERO, 10, Some(&starting));
        let mut seqs: Vec<i64> = set.iter_transactions().map(envelope_seq).collect();
        seqs.sort();
        // tx_a with seq 5 is filtered (starting_seq >= 5), only tx_b with seq 6 remains
        assert_eq!(seqs, vec![6]);
    }

    #[test]
    fn test_starting_sequence_boundary() {
        // With one-tx-per-account limit, use different accounts
        let queue = TransactionQueue::with_defaults();

        let starting_seq = (4_i64) << 32;
        let mut tx_starting = make_test_envelope(200, 1);
        let mut tx_next = make_test_envelope(200, 1);
        set_source(&mut tx_starting, 1);
        set_source(&mut tx_next, 2); // Different account
        if let TransactionEnvelope::Tx(env) = &mut tx_starting {
            env.tx.seq_num = SequenceNumber(starting_seq);
        }
        if let TransactionEnvelope::Tx(env) = &mut tx_next {
            env.tx.seq_num = SequenceNumber(starting_seq + 1);
        }

        queue.try_add(tx_starting.clone());
        queue.try_add(tx_next);

        // Set starting sequence for account 1, so tx_starting should be filtered out
        let account_id = account_id_from_envelope(&tx_starting);
        let mut starting = std::collections::HashMap::new();
        starting.insert(account_key_from_account_id(&account_id), starting_seq);

        let set = queue.get_transaction_set_with_starting_seq(Hash256::ZERO, 10, Some(&starting));
        let mut seqs: Vec<i64> = set.iter_transactions().map(envelope_seq).collect();
        seqs.sort();
        // tx_starting is filtered (starting_seq >= starting_seq), only tx_next remains
        assert_eq!(seqs, vec![starting_seq + 1]);
    }

    #[test]
    fn test_transaction_set_hash_matches_recompute() {
        let mut tx_a = make_test_envelope(200, 1);
        let mut tx_b = make_test_envelope(300, 1);
        set_source(&mut tx_a, 40);
        set_source(&mut tx_b, 41);
        if let TransactionEnvelope::Tx(env) = &mut tx_a {
            env.tx.seq_num = SequenceNumber(1);
        }
        if let TransactionEnvelope::Tx(env) = &mut tx_b {
            env.tx.seq_num = SequenceNumber(2);
        }

        let tx_set = TransactionSet::new(Hash256::ZERO, vec![tx_a, tx_b]);
        let recomputed = tx_set.recompute_hash();
        assert_eq!(*tx_set.hash(), recomputed);
    }

    #[test]
    fn test_generalized_tx_set_phase_split() {
        let queue = TransactionQueue::with_defaults();

        let classic = make_test_envelope(200, 1);
        let soroban = make_soroban_envelope(200);
        queue.try_add(classic.clone());
        queue.try_add(soroban.clone());

        let _set = queue.build_generalized_tx_set(Hash256::ZERO, 100);
        let gen = _set.generalized_tx_set().unwrap().clone();
        let stellar_xdr::GeneralizedTransactionSet::V1(v1) = gen;
        assert_eq!(v1.phases.len(), 2);

        match &v1.phases[0] {
            stellar_xdr::TransactionPhase::V0(components) => {
                let txs: Vec<_> = components
                    .iter()
                    .flat_map(|component| match component {
                        stellar_xdr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) => {
                            comp.txs.to_vec()
                        }
                    })
                    .collect();
                assert_eq!(txs.len(), 1);
                assert!(!henyey_tx::TransactionFrame::from_owned_with_network(
                    txs[0].clone(),
                    NetworkId::testnet()
                )
                .is_soroban());
            }
            _ => panic!("expected classic phase"),
        }

        match &v1.phases[1] {
            stellar_xdr::TransactionPhase::V1(parallel) => {
                let mut txs = Vec::new();
                for stage in parallel.execution_stages.iter() {
                    for cluster in stage.iter() {
                        txs.extend(cluster.0.iter().cloned());
                    }
                }
                assert_eq!(txs.len(), 1);
                assert!(henyey_tx::TransactionFrame::from_owned_with_network(
                    txs[0].clone(),
                    NetworkId::testnet()
                )
                .is_soroban());
            }
            _ => panic!("expected soroban phase"),
        }
    }

    #[test]
    fn test_generalized_tx_set_hash_matches_recompute() {
        let queue = TransactionQueue::with_defaults();

        let classic = make_test_envelope(200, 1);
        let soroban = make_soroban_envelope(200);
        queue.try_add(classic);
        queue.try_add(soroban);

        let tx_set = queue.build_generalized_tx_set(Hash256::ZERO, 100);
        let gen = tx_set.generalized_tx_set().unwrap().clone();
        let recomputed = tx_set.recompute_hash();
        assert_eq!(*tx_set.hash(), recomputed);

        let gen_hash = Hash256::hash_xdr(&gen);
        assert_eq!(*tx_set.hash(), gen_hash);
    }

    #[test]
    fn test_classic_base_fee_defaults_to_min_fee() {
        let queue = TransactionQueue::with_defaults();
        let expected_base_fee = queue.validation_context.read().base_fee as i64;

        queue.try_add(make_test_envelope(200, 1));
        queue.try_add(make_test_envelope(300, 1));

        let _set = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let gen = _set.generalized_tx_set().unwrap().clone();
        let stellar_xdr::GeneralizedTransactionSet::V1(v1) = gen;
        let base_fee = match &v1.phases[0] {
            stellar_xdr::TransactionPhase::V0(components) => {
                let stellar_xdr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) =
                    &components[0];
                comp.base_fee
            }
            _ => None,
        };

        assert_eq!(base_fee, Some(expected_base_fee));
    }

    #[test]
    fn test_soroban_base_fee_defaults_to_min_fee() {
        let queue = TransactionQueue::with_defaults();
        let expected_base_fee = queue.validation_context.read().base_fee as i64;

        queue.try_add(make_soroban_envelope(200));
        queue.try_add(make_soroban_envelope(300));

        let _set = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let gen = _set.generalized_tx_set().unwrap().clone();
        let stellar_xdr::GeneralizedTransactionSet::V1(v1) = gen;
        let base_fee = match &v1.phases[1] {
            stellar_xdr::TransactionPhase::V1(parallel) => parallel.base_fee,
            _ => None,
        };

        assert_eq!(base_fee, Some(expected_base_fee));
    }

    #[test]
    fn test_classic_component_orders_by_hash() {
        let queue = TransactionQueue::with_defaults();

        let mut tx_a = make_test_envelope(200, 1);
        let mut tx_b = make_test_envelope(200, 1);
        set_source(&mut tx_a, 11);
        set_source(&mut tx_b, 12);

        queue.try_add(tx_b.clone());
        queue.try_add(tx_a.clone());

        let _set = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let gen = _set.generalized_tx_set().unwrap().clone();
        let stellar_xdr::GeneralizedTransactionSet::V1(v1) = gen;

        let txs = match &v1.phases[0] {
            stellar_xdr::TransactionPhase::V0(components) => {
                let stellar_xdr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) =
                    &components[0];
                comp.txs.to_vec()
            }
            _ => panic!("expected classic phase"),
        };

        assert_eq!(txs.len(), 2);
        let hashes: Vec<_> = txs.iter().map(full_hash).collect();
        assert!(hashes[0].0 <= hashes[1].0);
    }

    #[test]
    fn test_soroban_component_orders_by_hash() {
        let queue = TransactionQueue::with_defaults();

        let mut tx_a = make_soroban_envelope(200);
        let mut tx_b = make_soroban_envelope(200);
        set_source(&mut tx_a, 21);
        set_source(&mut tx_b, 22);

        queue.try_add(tx_b.clone());
        queue.try_add(tx_a.clone());

        let _set = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let gen = _set.generalized_tx_set().unwrap().clone();
        let stellar_xdr::GeneralizedTransactionSet::V1(v1) = gen;

        let mut txs = Vec::new();
        match &v1.phases[1] {
            stellar_xdr::TransactionPhase::V1(parallel) => {
                for stage in parallel.execution_stages.iter() {
                    for cluster in stage.iter() {
                        txs.extend(cluster.0.iter().cloned());
                    }
                }
            }
            _ => panic!("expected soroban phase"),
        }

        assert_eq!(txs.len(), 2);
        let hashes: Vec<_> = txs.iter().map(full_hash).collect();
        assert!(hashes[0].0 <= hashes[1].0);
    }

    #[test]
    fn test_queue_rejects_below_current_base_fee() {
        let queue = TransactionQueue::with_defaults();

        queue.update_validation_context(
            1,
            0,
            25,
            500,
            5_000_000,
            0,
            std::time::Duration::from_secs(5),
        );

        let low_fee = make_test_envelope(200, 1);
        let high_fee = make_test_envelope(600, 1);

        assert_eq!(queue.try_add(low_fee), TxQueueResult::FeeTooLow);
        assert_eq!(queue.try_add(high_fee), TxQueueResult::Added);
    }

    #[test]
    fn test_classic_base_fee_surge() {
        let queue = TransactionQueue::with_defaults();

        let mut tx_low = make_test_envelope(8000, 80);
        let mut tx_high = make_test_envelope(12000, 80);
        set_source(&mut tx_low, 8);
        set_source(&mut tx_high, 9);
        assert_eq!(queue.try_add(tx_low), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_high), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        let SelectedTxs {
            classic_limited,
            transactions,
            ..
        } = queue.select_transactions(100);
        assert!(classic_limited);
        assert_eq!(transactions.len(), 1);

        let _set = queue.build_generalized_tx_set(Hash256::ZERO, 100);
        let gen = _set.generalized_tx_set().unwrap().clone();
        let stellar_xdr::GeneralizedTransactionSet::V1(v1) = gen;
        let base_fee = match &v1.phases[0] {
            stellar_xdr::TransactionPhase::V0(components) => match &components[0] {
                stellar_xdr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) => comp.base_fee,
            },
            _ => None,
        };

        assert_eq!(base_fee, Some(150));
    }

    #[test]
    fn test_classic_byte_limit() {
        let mut tx_high = make_test_envelope(400, 1);
        let mut tx_low = make_test_envelope(200, 1);
        set_source(&mut tx_high, 60);
        set_source(&mut tx_low, 61);

        let byte_limit = envelope_size(&tx_high) as u32;
        let config = TxQueueConfig {
            max_classic_bytes: Some(byte_limit),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        queue.try_add(tx_high);
        queue.try_add(tx_low);

        let SelectedTxs {
            classic_limited,
            transactions,
            ..
        } = queue.select_transactions(1000);
        assert!(classic_limited);
        assert_eq!(transactions.len(), 1);
        assert_eq!(envelope_fee(transactions[0].envelope()), 400);
    }

    #[test]
    fn test_queue_classic_byte_limit_eviction() {
        let mut tx_low = make_test_envelope(200, 1);
        let mut tx_high = make_test_envelope(400, 1);
        set_source(&mut tx_low, 62);
        set_source(&mut tx_high, 63);

        let byte_limit = envelope_size(&tx_high) as u32;
        let config = TxQueueConfig {
            max_queue_classic_bytes: Some(byte_limit),
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        assert_eq!(queue.try_add(tx_low.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_high.clone()), TxQueueResult::Added);

        let low_hash = full_hash(&tx_low);
        let high_hash = full_hash(&tx_high);
        assert!(!queue.contains(&low_hash));
        assert!(queue.contains(&high_hash));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_queue_classic_byte_limit_sets_min_fee_after_eviction() {
        let mut tx_low = make_test_envelope(200, 1);
        let mut tx_high = make_test_envelope(400, 1);
        let mut tx_lower = make_test_envelope(100, 1);
        set_source(&mut tx_low, 64);
        set_source(&mut tx_high, 65);
        set_source(&mut tx_lower, 66);

        let byte_limit = envelope_size(&tx_high) as u32;
        let config = TxQueueConfig {
            max_queue_classic_bytes: Some(byte_limit),
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        assert_eq!(queue.try_add(tx_low), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_high), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_lower), TxQueueResult::FeeTooLow);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_dex_ops_limit() {
        let config = TxQueueConfig {
            max_dex_ops: Some(1),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex_a = make_dex_envelope(400);
        let mut dex_b = make_dex_envelope(300);
        let mut classic = make_test_envelope(200, 1);
        set_source(&mut dex_a, 12);
        set_source(&mut dex_b, 13);
        set_source(&mut classic, 14);

        queue.try_add(dex_a);
        queue.try_add(dex_b);
        queue.try_add(classic);

        let set = queue.get_transaction_set(Hash256::ZERO, 10);
        assert_eq!(set.len(), 2);

        let mut dex_count = 0;
        for tx in set.iter_transactions() {
            let frame = henyey_tx::TransactionFrame::from_owned_with_network(
                tx.clone(),
                NetworkId::testnet(),
            );
            if frame.has_dex_operations() {
                dex_count += 1;
            }
        }
        assert_eq!(dex_count, 1);
    }

    #[test]
    fn test_dex_lane_limit_deterministic_selection() {
        let config = TxQueueConfig {
            max_dex_ops: Some(1),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex_a = make_dex_envelope(200);
        let mut dex_b = make_dex_envelope(200);
        let mut classic = make_test_envelope(200, 1);
        set_source(&mut dex_a, 201);
        set_source(&mut dex_b, 202);
        set_source(&mut classic, 203);

        queue.try_add(dex_a.clone());
        queue.try_add(dex_b.clone());
        queue.try_add(classic.clone());

        let set = queue.get_transaction_set(Hash256::ZERO, 10);
        assert_eq!(set.len(), 2);

        let hash_dex_a = full_hash(&dex_a);
        let hash_dex_b = full_hash(&dex_b);
        let hash_classic = full_hash(&classic);
        let included_dex = if hash_dex_a.0 <= hash_dex_b.0 {
            hash_dex_a
        } else {
            hash_dex_b
        };

        let mut expected = vec![hash_classic, included_dex];
        expected.sort_by_key(|a| a.0);
        let hashes: Vec<_> = set.iter_transactions().map(full_hash).collect();
        assert_eq!(hashes, expected);
    }

    #[test]
    fn test_dex_limit_sets_only_dex_limited_flag() {
        let config = TxQueueConfig {
            max_dex_ops: Some(1),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex_a = make_dex_envelope(400);
        let mut dex_b = make_dex_envelope(300);
        let mut classic = make_test_envelope(200, 1);
        set_source(&mut dex_a, 16);
        set_source(&mut dex_b, 17);
        set_source(&mut classic, 18);

        queue.try_add(dex_a);
        queue.try_add(dex_b);
        queue.try_add(classic);

        let SelectedTxs {
            dex_limited,
            classic_limited,
            transactions,
            ..
        } = queue.select_transactions(10);

        assert!(dex_limited);
        assert!(!classic_limited);
        assert_eq!(transactions.len(), 2);
    }

    #[test]
    fn test_dex_evicts_non_dex_when_lane_insufficient() {
        let config = TxQueueConfig {
            max_queue_ops: Some(9),
            max_queue_dex_ops: Some(3),
            max_size: 10,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut non_dex = make_test_envelope(100 * 8, 8);
        let mut dex_low = make_dex_envelope(200);
        let mut dex_high = make_dex_envelope_with_ops(10000 * 3, 3);
        set_source(&mut non_dex, 100);
        set_source(&mut dex_low, 101);
        set_source(&mut dex_high, 102);

        let non_dex_hash = full_hash(&non_dex);
        let dex_low_hash = full_hash(&dex_low);
        let dex_high_hash = full_hash(&dex_high);

        assert_eq!(queue.try_add(non_dex), TxQueueResult::Added);
        assert_eq!(queue.try_add(dex_low), TxQueueResult::Added);
        assert_eq!(queue.try_add(dex_high), TxQueueResult::Added);

        assert!(!queue.contains(&non_dex_hash));
        assert!(!queue.contains(&dex_low_hash));
        assert!(queue.contains(&dex_high_hash));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_dex_eviction_with_global_limit_only() {
        let config = TxQueueConfig {
            max_queue_ops: Some(9),
            max_queue_dex_ops: Some(3),
            max_size: 10,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex = make_dex_envelope_with_ops(200, 1);
        let mut non_dex_high = make_test_envelope(400 * 6, 6);
        let mut non_dex_low = make_test_envelope(100, 1);
        let mut non_dex_mid = make_test_envelope(300, 1);
        let mut dex_new = make_dex_envelope_with_ops(301 * 3, 3);
        set_source(&mut dex, 110);
        set_source(&mut non_dex_high, 111);
        set_source(&mut non_dex_low, 112);
        set_source(&mut non_dex_mid, 113);
        set_source(&mut dex_new, 114);

        let dex_hash = full_hash(&dex);
        let non_dex_high_hash = full_hash(&non_dex_high);
        let non_dex_low_hash = full_hash(&non_dex_low);
        let non_dex_mid_hash = full_hash(&non_dex_mid);
        let dex_new_hash = full_hash(&dex_new);

        assert_eq!(queue.try_add(dex), TxQueueResult::Added);
        assert_eq!(queue.try_add(non_dex_high), TxQueueResult::Added);
        assert_eq!(queue.try_add(non_dex_low), TxQueueResult::Added);
        assert_eq!(queue.try_add(non_dex_mid), TxQueueResult::Added);
        assert_eq!(queue.try_add(dex_new), TxQueueResult::Added);

        assert!(!queue.contains(&dex_hash));
        assert!(queue.contains(&non_dex_high_hash));
        assert!(!queue.contains(&non_dex_low_hash));
        assert!(!queue.contains(&non_dex_mid_hash));
        assert!(queue.contains(&dex_new_hash));
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn test_dex_eviction_with_global_and_dex_limits() {
        let config = TxQueueConfig {
            max_queue_ops: Some(9),
            max_queue_dex_ops: Some(3),
            max_size: 10,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex = make_dex_envelope_with_ops(200 * 2, 2);
        let mut non_dex_high = make_test_envelope(400 * 5, 5);
        let mut non_dex_low = make_test_envelope(100, 1);
        let mut non_dex_mid = make_test_envelope(150, 1);
        let mut dex_new = make_dex_envelope_with_ops(201 * 3, 3);
        set_source(&mut dex, 120);
        set_source(&mut non_dex_high, 121);
        set_source(&mut non_dex_low, 122);
        set_source(&mut non_dex_mid, 123);
        set_source(&mut dex_new, 124);

        let dex_hash = full_hash(&dex);
        let non_dex_high_hash = full_hash(&non_dex_high);
        let non_dex_low_hash = full_hash(&non_dex_low);
        let non_dex_mid_hash = full_hash(&non_dex_mid);
        let dex_new_hash = full_hash(&dex_new);

        assert_eq!(queue.try_add(dex), TxQueueResult::Added);
        assert_eq!(queue.try_add(non_dex_high), TxQueueResult::Added);
        assert_eq!(queue.try_add(non_dex_low), TxQueueResult::Added);
        assert_eq!(queue.try_add(non_dex_mid), TxQueueResult::Added);
        assert_eq!(queue.try_add(dex_new), TxQueueResult::Added);

        assert!(!queue.contains(&dex_hash));
        assert!(queue.contains(&non_dex_high_hash));
        assert!(!queue.contains(&non_dex_low_hash));
        assert!(queue.contains(&non_dex_mid_hash));
        assert!(queue.contains(&dex_new_hash));
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn test_dex_only_min_fee_threshold_after_eviction() {
        let config = TxQueueConfig {
            max_queue_ops: Some(9),
            max_queue_dex_ops: Some(3),
            max_size: 10,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex_low_a = make_dex_envelope_with_ops(100, 1);
        let mut dex_mid = make_dex_envelope_with_ops(150 * 2, 2);
        let mut dex_evicted = make_dex_envelope_with_ops(200 * 2, 2);
        let mut dex_low = make_dex_envelope_with_ops(100, 1);
        let mut dex_high = make_dex_envelope_with_ops(201 * 3, 3);
        set_source(&mut dex_low_a, 140);
        set_source(&mut dex_mid, 141);
        set_source(&mut dex_evicted, 142);
        set_source(&mut dex_low, 143);
        set_source(&mut dex_high, 144);

        assert_eq!(queue.try_add(dex_low_a), TxQueueResult::Added);
        assert_eq!(queue.try_add(dex_mid), TxQueueResult::Added);
        assert_eq!(queue.try_add(dex_evicted), TxQueueResult::Added);
        assert_eq!(queue.try_add(dex_low), TxQueueResult::FeeTooLow);
        assert_eq!(queue.try_add(dex_high), TxQueueResult::Added);
    }

    #[test]
    fn test_non_dex_only_min_fee_threshold_after_eviction() {
        let config = TxQueueConfig {
            max_queue_ops: Some(6),
            max_queue_dex_ops: Some(3),
            max_size: 10,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut non_dex_a = make_test_envelope(100, 1);
        let mut non_dex_b = make_test_envelope(150 * 5, 5);
        let mut non_dex_evict = make_test_envelope(200 * 5, 5);
        let mut non_dex_low = make_test_envelope(100, 1);
        let mut non_dex_high = make_test_envelope(201 * 2, 2);
        set_source(&mut non_dex_a, 150);
        set_source(&mut non_dex_b, 151);
        set_source(&mut non_dex_evict, 152);
        set_source(&mut non_dex_low, 153);
        set_source(&mut non_dex_high, 154);

        assert_eq!(queue.try_add(non_dex_a), TxQueueResult::Added);
        assert_eq!(queue.try_add(non_dex_b), TxQueueResult::Added);
        assert_eq!(queue.try_add(non_dex_evict), TxQueueResult::Added);
        assert_eq!(queue.try_add(non_dex_low), TxQueueResult::FeeTooLow);
        assert_eq!(queue.try_add(non_dex_high), TxQueueResult::Added);
    }

    #[test]
    fn test_classic_components_group_by_discounted_base_fee() {
        let mut dex_a = make_dex_envelope(300);
        let mut dex_b = make_dex_envelope(200);
        let mut classic_high = make_test_envelope(250, 1);
        let mut classic_low = make_test_envelope(100, 1);
        set_source(&mut dex_a, 160);
        set_source(&mut dex_b, 161);
        set_source(&mut classic_high, 162);
        set_source(&mut classic_low, 163);

        let byte_limit = (envelope_size(&dex_a) + envelope_size(&classic_high)) as u32;
        let config = TxQueueConfig {
            max_dex_ops: Some(1),
            max_classic_bytes: Some(byte_limit),
            max_size: 10,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        queue.try_add(dex_a.clone());
        queue.try_add(dex_b.clone());
        queue.try_add(classic_high.clone());
        queue.try_add(classic_low.clone());

        let _set = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let gen = _set.generalized_tx_set().unwrap().clone();
        let stellar_xdr::GeneralizedTransactionSet::V1(tx_set) = gen;
        let phases = &tx_set.phases;
        let components = match &phases[0] {
            stellar_xdr::TransactionPhase::V0(components) => components,
            _ => panic!("expected classic phase"),
        };
        assert_eq!(components.len(), 2);

        let mut base_fees = Vec::new();
        let mut tx_counts = Vec::new();
        for comp in components.iter() {
            let stellar_xdr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = comp;
            base_fees.push(comp.base_fee);
            tx_counts.push(comp.txs.len());
        }
        base_fees.sort();
        tx_counts.sort();
        assert_eq!(base_fees, vec![Some(250), Some(300)]);
        assert_eq!(tx_counts, vec![1, 1]);
    }

    #[test]
    fn test_dex_and_non_dex_min_fee_thresholds_after_evictions() {
        let config = TxQueueConfig {
            max_queue_ops: Some(9),
            max_queue_dex_ops: Some(3),
            max_size: 10,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex_a = make_dex_envelope_with_ops(200 * 2, 2);
        let mut non_dex_a = make_test_envelope(100 * 3, 3);
        let mut dex_b = make_dex_envelope_with_ops(300 * 2, 2);
        let mut non_dex_b = make_test_envelope(250 * 5, 5);
        set_source(&mut dex_a, 130);
        set_source(&mut non_dex_a, 131);
        set_source(&mut dex_b, 132);
        set_source(&mut non_dex_b, 133);

        let dex_a_hash = full_hash(&dex_a);
        let non_dex_a_hash = full_hash(&non_dex_a);
        let dex_b_hash = full_hash(&dex_b);
        let non_dex_b_hash = full_hash(&non_dex_b);

        assert_eq!(queue.try_add(dex_a), TxQueueResult::Added);
        assert_eq!(queue.try_add(non_dex_a), TxQueueResult::Added);
        assert_eq!(queue.try_add(dex_b), TxQueueResult::Added);
        assert_eq!(queue.try_add(non_dex_b), TxQueueResult::Added);

        assert!(!queue.contains(&dex_a_hash));
        assert!(!queue.contains(&non_dex_a_hash));
        assert!(queue.contains(&dex_b_hash));
        assert!(queue.contains(&non_dex_b_hash));

        let mut dex_low = make_dex_envelope_with_ops(200, 1);
        let mut non_dex_low = make_test_envelope(100, 1);
        let mut dex_high = make_dex_envelope_with_ops(201, 1);
        let mut non_dex_high = make_test_envelope(101, 1);
        set_source(&mut dex_low, 134);
        set_source(&mut non_dex_low, 135);
        set_source(&mut dex_high, 136);
        set_source(&mut non_dex_high, 137);

        assert_eq!(queue.try_add(dex_low), TxQueueResult::FeeTooLow);
        assert_eq!(queue.try_add(non_dex_low), TxQueueResult::FeeTooLow);
        assert_eq!(queue.try_add(dex_high), TxQueueResult::Added);
        assert_eq!(queue.try_add(non_dex_high), TxQueueResult::Added);
    }

    #[test]
    fn test_dex_queue_limit_eviction() {
        let config = TxQueueConfig {
            max_queue_dex_ops: Some(1),
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex_low = make_dex_envelope(200);
        let mut dex_high = make_dex_envelope(400);
        set_source(&mut dex_low, 21);
        set_source(&mut dex_high, 22);

        assert_eq!(queue.try_add(dex_low.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(dex_high.clone()), TxQueueResult::Added);

        let low_hash = full_hash(&dex_low);
        let high_hash = full_hash(&dex_high);
        assert!(!queue.contains(&low_hash));
        assert!(queue.contains(&high_hash));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_dex_queue_limit_sets_min_fee_after_eviction() {
        let config = TxQueueConfig {
            max_queue_dex_ops: Some(1),
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex_low = make_dex_envelope(200);
        let mut dex_high = make_dex_envelope(400);
        let mut dex_lower = make_dex_envelope(150);
        set_source(&mut dex_low, 31);
        set_source(&mut dex_high, 32);
        set_source(&mut dex_lower, 33);

        assert_eq!(queue.try_add(dex_low), TxQueueResult::Added);
        assert_eq!(queue.try_add(dex_high), TxQueueResult::Added);
        assert_eq!(queue.try_add(dex_lower), TxQueueResult::FeeTooLow);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_dex_lane_min_fee_blocks_classic() {
        let config = TxQueueConfig {
            max_queue_dex_ops: Some(1),
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex_low = make_dex_envelope(200);
        let mut dex_high = make_dex_envelope(400);
        let mut classic_low = make_test_envelope(100, 1);
        set_source(&mut dex_low, 34);
        set_source(&mut dex_high, 35);
        set_source(&mut classic_low, 36);

        assert_eq!(queue.try_add(dex_low), TxQueueResult::Added);
        assert_eq!(queue.try_add(dex_high), TxQueueResult::Added);
        assert_eq!(queue.try_add(classic_low), TxQueueResult::FeeTooLow);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_queue_ops_limit_eviction() {
        let config = TxQueueConfig {
            max_queue_ops: Some(2),
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx_low = make_test_envelope(100, 1);
        let mut tx_mid = make_test_envelope(200, 1);
        let mut tx_high = make_test_envelope(400, 1);
        set_source(&mut tx_low, 31);
        set_source(&mut tx_mid, 32);
        set_source(&mut tx_high, 33);

        assert_eq!(queue.try_add(tx_low.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_mid.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_high.clone()), TxQueueResult::Added);

        let low_hash = full_hash(&tx_low);
        let mid_hash = full_hash(&tx_mid);
        let high_hash = full_hash(&tx_high);
        assert!(!queue.contains(&low_hash));
        assert!(queue.contains(&mid_hash));
        assert!(queue.contains(&high_hash));
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn test_queue_ops_limit_sets_min_fee_after_eviction() {
        let config = TxQueueConfig {
            max_queue_ops: Some(2),
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx_low = make_test_envelope(100, 1);
        let mut tx_high = make_test_envelope(400, 1);
        let mut tx_lower = make_test_envelope(80, 1);
        set_source(&mut tx_low, 41);
        set_source(&mut tx_high, 42);
        set_source(&mut tx_lower, 43);

        assert_eq!(queue.try_add(tx_low), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_high), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_lower), TxQueueResult::FeeTooLow);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn test_queue_ops_limit_accepts_higher_fee_after_eviction() {
        let config = TxQueueConfig {
            max_queue_ops: Some(2),
            max_size: 10,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx_low = make_test_envelope(100, 1);
        let mut tx_high = make_test_envelope(400, 1);
        let mut tx_mid = make_test_envelope(150, 1);
        set_source(&mut tx_low, 52);
        set_source(&mut tx_high, 53);
        set_source(&mut tx_mid, 54);

        let low_hash = full_hash(&tx_low);
        let high_hash = full_hash(&tx_high);
        let mid_hash = full_hash(&tx_mid);

        assert_eq!(queue.try_add(tx_low), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_high), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_mid), TxQueueResult::Added);

        assert!(!queue.contains(&low_hash));
        assert!(queue.contains(&high_hash));
        assert!(queue.contains(&mid_hash));
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn test_eviction_thresholds_reset_after_age_eviction() {
        let config = TxQueueConfig {
            max_queue_ops: Some(1),
            max_size: 10,
            max_age_secs: 1,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx_low = make_test_envelope(100, 1);
        let mut tx_high = make_test_envelope(200, 1);
        let mut tx_lower = make_test_envelope(80, 1);
        let mut tx_new = make_test_envelope(80, 1);
        set_source(&mut tx_low, 90);
        set_source(&mut tx_high, 91);
        set_source(&mut tx_lower, 92);
        set_source(&mut tx_new, 93);

        assert_eq!(queue.try_add(tx_low), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_high), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_lower), TxQueueResult::FeeTooLow);
        assert_eq!(queue.len(), 1);

        {
            let mut store = queue.store.write();
            for tx in store.values_mut() {
                tx.received_at = tx
                    .received_at
                    .checked_sub(std::time::Duration::from_secs(10))
                    .unwrap_or_else(|| Instant::now() - std::time::Duration::from_secs(10));
            }
        }
        queue.evict_expired();
        assert!(queue.is_empty());

        assert_eq!(queue.try_add(tx_new), TxQueueResult::Added);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_queue_ops_limit_rejects_same_account_eviction() {
        // With one-tx-per-account limit, the second transaction is rejected with TryAgainLater
        // (not QueueFull) because the account already has a pending transaction.
        let config = TxQueueConfig {
            max_queue_ops: Some(1),
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx_low = make_test_envelope(100, 1);
        let mut tx_high = make_test_envelope(400, 1);
        set_source(&mut tx_low, 91);
        set_source(&mut tx_high, 91);
        if let TransactionEnvelope::Tx(env) = &mut tx_low {
            env.tx.seq_num = SequenceNumber(1);
        }
        if let TransactionEnvelope::Tx(env) = &mut tx_high {
            env.tx.seq_num = SequenceNumber(2);
        }

        assert_eq!(queue.try_add(tx_low), TxQueueResult::Added);
        // With one-tx-per-account, second tx from same account is rejected as TryAgainLater
        assert_eq!(queue.try_add(tx_high), TxQueueResult::TryAgainLater);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_dex_base_fee_override() {
        let config = TxQueueConfig {
            max_dex_ops: Some(1),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);
        let base_fee = queue.validation_context.read().base_fee as i64;

        let mut dex_high = make_dex_envelope(500);
        let mut dex_low = make_dex_envelope(300);
        let mut classic = make_test_envelope(200, 1);
        set_source(&mut dex_high, 51);
        set_source(&mut dex_low, 52);
        set_source(&mut classic, 53);

        queue.try_add(dex_high);
        queue.try_add(dex_low);
        queue.try_add(classic);

        let _set = queue.build_generalized_tx_set(Hash256::ZERO, 200);
        let gen = _set.generalized_tx_set().unwrap().clone();
        let stellar_xdr::GeneralizedTransactionSet::V1(v1) = gen;
        let components = match &v1.phases[0] {
            stellar_xdr::TransactionPhase::V0(comps) => comps,
            _ => panic!("expected classic phase"),
        };

        let mut has_dex_fee = false;
        let mut has_classic_fee = false;
        for comp in components.iter() {
            let stellar_xdr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = comp;
            match comp.base_fee {
                Some(500) => has_dex_fee = true,
                Some(fee) if fee == base_fee => has_classic_fee = true,
                _ => {}
            }
        }
        assert!(has_dex_fee);
        assert!(has_classic_fee);
    }

    #[test]
    fn test_soroban_queue_limit_eviction() {
        let mut limit = Resource::new(vec![i64::MAX; NUM_SOROBAN_TX_RESOURCES]);
        limit.set_val(ResourceType::Instructions, 100);
        let config = TxQueueConfig {
            max_queue_soroban_resources: Some(limit),
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut low_fee = make_soroban_envelope_with_resources(4000, 80);
        let mut high_fee = make_soroban_envelope_with_resources(8000, 80);
        set_source(&mut low_fee, 71);
        set_source(&mut high_fee, 72);

        assert_eq!(queue.try_add(low_fee.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(high_fee.clone()), TxQueueResult::Added);

        let low_hash = full_hash(&low_fee);
        let high_hash = full_hash(&high_fee);
        assert!(!queue.contains(&low_hash));
        assert!(queue.contains(&high_hash));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_soroban_queue_limit_sets_min_fee_after_eviction() {
        let mut limit = Resource::new(vec![i64::MAX; NUM_SOROBAN_TX_RESOURCES]);
        limit.set_val(ResourceType::Instructions, 100);
        let config = TxQueueConfig {
            max_queue_soroban_resources: Some(limit),
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut low_fee = make_soroban_envelope_with_resources(4000, 80);
        let mut high_fee = make_soroban_envelope_with_resources(8000, 80);
        let mut lower_fee = make_soroban_envelope_with_resources(2000, 80);
        set_source(&mut low_fee, 81);
        set_source(&mut high_fee, 82);
        set_source(&mut lower_fee, 83);

        assert_eq!(queue.try_add(low_fee), TxQueueResult::Added);
        assert_eq!(queue.try_add(high_fee), TxQueueResult::Added);
        assert_eq!(queue.try_add(lower_fee), TxQueueResult::FeeTooLow);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_soroban_resource_limit() {
        let mut limit = Resource::new(vec![i64::MAX; NUM_SOROBAN_TX_RESOURCES]);
        limit.set_val(ResourceType::Instructions, 100);
        let config = TxQueueConfig {
            max_soroban_resources: Some(limit),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx_a = make_soroban_envelope_with_resources(400, 80);
        let mut tx_b = make_soroban_envelope_with_resources(300, 80);
        set_source(&mut tx_a, 31);
        set_source(&mut tx_b, 32);
        queue.try_add(tx_a);
        queue.try_add(tx_b);

        let set = queue.get_transaction_set(Hash256::ZERO, 10);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn test_soroban_base_fee_on_limit() {
        let mut limit = Resource::new(vec![i64::MAX; NUM_SOROBAN_TX_RESOURCES]);
        limit.set_val(ResourceType::Instructions, 100);
        let config = TxQueueConfig {
            max_soroban_resources: Some(limit),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut high_fee = make_soroban_envelope_with_resources(8000, 80);
        let mut low_fee = make_soroban_envelope_with_resources(4000, 80);
        set_source(&mut high_fee, 41);
        set_source(&mut low_fee, 42);
        queue.try_add(high_fee);
        queue.try_add(low_fee);

        let _set = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let gen = _set.generalized_tx_set().unwrap().clone();
        let stellar_xdr::GeneralizedTransactionSet::V1(v1) = gen;
        let base_fee = match &v1.phases[1] {
            stellar_xdr::TransactionPhase::V1(parallel) => parallel.base_fee,
            _ => None,
        };
        assert_eq!(base_fee, Some(8000));
    }

    #[test]
    fn test_audit_018_soroban_selection_uses_inclusion_fee() {
        let mut limit = Resource::new(vec![i64::MAX; NUM_SOROBAN_TX_RESOURCES]);
        limit.set_val(ResourceType::Instructions, 100);
        let config = TxQueueConfig {
            max_soroban_resources: Some(limit),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut low_inclusion = make_soroban_envelope_with_resource_fee(1000, 900, 50);
        let mut highest_inclusion = make_soroban_envelope_with_resource_fee(900, 0, 50);
        let mut next_highest_inclusion = make_soroban_envelope_with_resource_fee(800, 0, 50);
        set_source(&mut low_inclusion, 51);
        set_source(&mut highest_inclusion, 52);
        set_source(&mut next_highest_inclusion, 53);

        queue.try_add(low_inclusion.clone());
        queue.try_add(highest_inclusion.clone());
        queue.try_add(next_highest_inclusion.clone());

        let _set = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let gen = _set.generalized_tx_set().unwrap().clone();
        let stellar_xdr::GeneralizedTransactionSet::V1(v1) = gen;
        let parallel = match &v1.phases[1] {
            stellar_xdr::TransactionPhase::V1(parallel) => parallel,
            _ => panic!("expected Soroban V1 phase"),
        };

        let selected: Vec<_> = parallel
            .execution_stages
            .iter()
            .flat_map(|stage| stage.iter())
            .flat_map(|cluster| cluster.iter().cloned())
            .collect();

        assert_eq!(selected.len(), 2);
        assert!(selected.contains(&highest_inclusion));
        assert!(selected.contains(&next_highest_inclusion));
        assert!(!selected.contains(&low_inclusion));
        assert_eq!(parallel.base_fee, Some(800));
    }

    #[test]
    fn test_soroban_byte_limit() {
        let mut limit = Resource::new(vec![i64::MAX; NUM_SOROBAN_TX_RESOURCES]);
        limit.set_val(ResourceType::TxByteSize, i64::MAX);
        let mut tx_high = make_soroban_envelope(12000);
        let mut tx_low = make_soroban_envelope(8000);
        set_source(&mut tx_high, 71);
        set_source(&mut tx_low, 72);
        let tx_size = envelope_size(&tx_high) as u32;
        let config = TxQueueConfig {
            max_soroban_resources: Some(limit),
            max_soroban_bytes: Some(tx_size),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        queue.try_add(tx_high);
        queue.try_add(tx_low);

        let SelectedTxs {
            soroban_limited,
            transactions,
            ..
        } = queue.select_transactions(1000);
        assert!(soroban_limited);
        assert_eq!(transactions.len(), 1);
        assert_eq!(envelope_fee(transactions[0].envelope()), 12000);
    }

    #[test]
    fn test_soroban_byte_limit_without_resource_limit() {
        let mut tx_high = make_soroban_envelope(12000);
        let mut tx_low = make_soroban_envelope(8000);
        set_source(&mut tx_high, 81);
        set_source(&mut tx_low, 82);
        let tx_size = envelope_size(&tx_high) as u32;
        let config = TxQueueConfig {
            max_soroban_bytes: Some(tx_size),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        queue.try_add(tx_high);
        queue.try_add(tx_low);

        let SelectedTxs {
            soroban_limited,
            transactions,
            ..
        } = queue.select_transactions(1000);
        assert!(soroban_limited);
        assert_eq!(transactions.len(), 1);
        assert_eq!(envelope_fee(transactions[0].envelope()), 12000);
    }

    #[test]
    fn test_soroban_no_limit_order_is_deterministic() {
        let config = TxQueueConfig {
            max_soroban_resources: None,
            max_soroban_bytes: None,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx_a = make_soroban_envelope(5000);
        let mut tx_b = make_soroban_envelope(4000);
        set_source(&mut tx_a, 1);
        set_source(&mut tx_b, 2);
        queue.try_add(tx_b);
        queue.try_add(tx_a);

        let SelectedTxs { transactions, .. } = queue.select_transactions(1000);
        assert_eq!(transactions.len(), 2);
        let key_a = account_key(transactions[0].envelope());
        let key_b = account_key(transactions[1].envelope());
        assert!(key_a < key_b);
    }

    #[test]
    fn test_queue_full() {
        // With one-tx-per-account limit, use different accounts for each transaction
        let config = TxQueueConfig {
            max_size: 2,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx1 = make_test_envelope(100, 1);
        let mut tx2 = make_test_envelope(200, 1);
        let mut tx3 = make_test_envelope(300, 1);
        set_source(&mut tx1, 1);
        set_source(&mut tx2, 2);
        set_source(&mut tx3, 3);

        queue.try_add(tx1);
        queue.try_add(tx2);
        // Third transaction should evict the lowest-fee one
        let result = queue.try_add(tx3);
        assert_eq!(result, TxQueueResult::Added);
    }

    #[test]
    fn test_queue_eviction_for_higher_fee() {
        let config = TxQueueConfig {
            max_size: 1,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut low = make_test_envelope(200, 1);
        let mut high = make_test_envelope(400, 1);
        set_source(&mut low, 21);
        set_source(&mut high, 22);

        let low_hash = full_hash(&low);
        let high_hash = full_hash(&high);

        assert_eq!(queue.try_add(low), TxQueueResult::Added);
        assert_eq!(queue.try_add(high), TxQueueResult::Added);
        assert!(!queue.contains(&low_hash));
        assert!(queue.contains(&high_hash));
    }

    #[test]
    fn test_remove_applied() {
        let queue = TransactionQueue::with_defaults();

        let mut tx = make_test_envelope(200, 1);
        set_source(&mut tx, 44);
        queue.try_add(tx.clone());

        let hash = full_hash(&tx);
        assert!(queue.contains(&hash));

        queue.remove_applied(&[(tx, 1)]);
        assert!(!queue.contains(&hash));
        assert_eq!(queue.len(), 0);
    }

    /// After remove_applied, the account_states entry must be fully
    /// cleaned up (transaction cleared, fees released, empty entry removed)
    /// so the account can immediately submit a new transaction.
    #[test]
    fn test_remove_applied_clears_account_state() {
        let queue = TransactionQueue::with_defaults();

        let mut tx = make_test_envelope(200, 1);
        set_source(&mut tx, 50);

        assert_eq!(queue.try_add(tx.clone()), TxQueueResult::Added);
        // Verify account_state was created
        assert!(queue.account_states.read().contains_key(&account_key(&tx)));

        queue.remove_applied(&[(tx.clone(), 1)]);

        // Account state should be fully cleaned up (empty entry removed)
        assert!(
            !queue.account_states.read().contains_key(&account_key(&tx)),
            "empty account_state entry should be removed"
        );
    }

    /// After remove_applied, a new transaction from the same source
    /// must be accepted (not rejected with TryAgainLater).
    #[test]
    fn test_remove_applied_allows_new_tx_from_same_account() {
        let queue = TransactionQueue::with_defaults();

        let mut tx1 = make_test_envelope(200, 1);
        set_source(&mut tx1, 60);

        assert_eq!(queue.try_add(tx1.clone()), TxQueueResult::Added);
        queue.remove_applied(&[(tx1, 1)]);

        // A new tx from the same account should be accepted
        let mut tx2 = make_test_envelope(300, 1);
        set_source(&mut tx2, 60);
        assert_eq!(
            queue.try_add(tx2),
            TxQueueResult::Added,
            "new tx from same account should not get TryAgainLater"
        );
    }

    /// Sequence-based removal: a queued tx with seq=1 should be removed when
    /// an applied tx with seq=7 for the same account is processed.
    #[test]
    fn test_remove_applied_sequence_based_supersedes() {
        let queue = TransactionQueue::with_defaults();

        let mut queued_tx = make_test_envelope(200, 1);
        set_source(&mut queued_tx, 42);
        assert_eq!(queue.try_add(queued_tx.clone()), TxQueueResult::Added);
        let queued_hash = full_hash(&queued_tx);
        assert!(queue.contains(&queued_hash));

        let mut applied_tx = make_test_envelope(300, 1);
        set_source(&mut applied_tx, 42);
        if let TransactionEnvelope::Tx(ref mut env) = applied_tx {
            env.tx.seq_num = SequenceNumber(7);
        }

        queue.remove_applied(&[(applied_tx.clone(), 7)]);

        assert!(
            !queue.contains(&queued_hash),
            "queued tx with seq=1 should be removed when applied tx has seq=7"
        );
        assert_eq!(queue.len(), 0);
    }

    /// #3719 self-heal: a pending tx whose sequence the ledger has already
    /// consumed is dropped by drop_stale_pending, and the account can then
    /// admit its next tx (previously TryAgainLater until remove_applied /
    /// revalidation caught up).
    #[test]
    fn test_drop_stale_pending_unblocks_account() {
        let queue = TransactionQueue::with_defaults();

        let mut stale = make_test_envelope(200, 1);
        set_source(&mut stale, 44);
        if let TransactionEnvelope::Tx(ref mut env) = stale {
            env.tx.seq_num = SequenceNumber(7);
        }
        assert_eq!(queue.try_add(stale.clone()), TxQueueResult::Added);
        let account = account_id_from_envelope(&stale);
        let stale_hash = full_hash(&stale);

        // The bug scenario: account already at seq 7 on-ledger; the next
        // submission (seq 8) is rejected because of the stale pending.
        let mut next = make_test_envelope(200, 1);
        set_source(&mut next, 44);
        if let TransactionEnvelope::Tx(ref mut env) = next {
            env.tx.seq_num = SequenceNumber(8);
        }
        assert_eq!(queue.try_add(next.clone()), TxQueueResult::TryAgainLater);

        // Self-heal: drop the stale entry, then the next tx admits.
        assert!(queue.drop_stale_pending(&account, 7));
        assert!(!queue.contains(&stale_hash), "stale entry must be removed");
        assert_eq!(queue.try_add(next), TxQueueResult::Added);
        assert_eq!(queue.len(), 1);
    }

    /// drop_stale_pending must NOT touch a genuinely-pending tx (seq above
    /// the account's on-ledger seq).
    #[test]
    fn test_drop_stale_pending_keeps_live_pending() {
        let queue = TransactionQueue::with_defaults();

        let mut pending = make_test_envelope(200, 1);
        set_source(&mut pending, 45);
        if let TransactionEnvelope::Tx(ref mut env) = pending {
            env.tx.seq_num = SequenceNumber(8);
        }
        assert_eq!(queue.try_add(pending.clone()), TxQueueResult::Added);
        let account = account_id_from_envelope(&pending);
        let hash = full_hash(&pending);

        // Account at seq 7: the seq-8 pending is live.
        assert!(!queue.drop_stale_pending(&account, 7));
        assert!(queue.contains(&hash), "live pending must be kept");
    }

    /// drop_stale_pending releases the reserved fee so subsequent
    /// fee-accounting starts clean (mirrors remove_applied bookkeeping).
    #[test]
    fn test_drop_stale_pending_releases_fee_state() {
        let queue = TransactionQueue::with_defaults();

        let mut stale = make_test_envelope(200, 1);
        set_source(&mut stale, 46);
        assert_eq!(queue.try_add(stale.clone()), TxQueueResult::Added);
        let account = account_id_from_envelope(&stale);

        assert!(queue.drop_stale_pending(&account, 1));
        assert!(
            !queue
                .account_states
                .read()
                .contains_key(&account_key(&stale)),
            "account state must be fully cleaned up after the stale drop"
        );
        assert_eq!(queue.len(), 0);
    }

    /// Sequence-based removal should NOT remove a queued tx whose seq_num
    /// is higher than the applied one.
    #[test]
    fn test_remove_applied_sequence_based_no_supersede_higher_seq() {
        let queue = TransactionQueue::with_defaults();

        let mut queued_tx = make_test_envelope(200, 1);
        set_source(&mut queued_tx, 43);
        if let TransactionEnvelope::Tx(ref mut env) = queued_tx {
            env.tx.seq_num = SequenceNumber(10);
        }
        assert_eq!(queue.try_add(queued_tx.clone()), TxQueueResult::Added);
        let queued_hash = full_hash(&queued_tx);

        let mut applied_tx = make_test_envelope(300, 1);
        set_source(&mut applied_tx, 43);
        if let TransactionEnvelope::Tx(ref mut env) = applied_tx {
            env.tx.seq_num = SequenceNumber(5);
        }

        queue.remove_applied(&[(applied_tx.clone(), 5)]);

        assert!(
            queue.contains(&queued_hash),
            "queued tx with seq=10 should NOT be removed when applied tx has seq=5"
        );
        assert_eq!(queue.len(), 1);
    }

    /// Helper: wrap a regular envelope in a fee-bump with a different fee source.
    fn make_fee_bump_envelope(
        inner: TransactionV1Envelope,
        fee_source_seed: u8,
        outer_fee: i64,
    ) -> TransactionEnvelope {
        let fee_source = MuxedAccount::Ed25519(Uint256([fee_source_seed; 32]));
        TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: FeeBumpTransaction {
                fee_source,
                fee: outer_fee,
                inner_tx: FeeBumpTransactionInnerTx::Tx(inner),
                ext: FeeBumpTransactionExt::V0,
            },
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    /// Fee-bump removal: remove_applied uses inner source for sequence matching
    /// and outer source (fee_source) for fee release.
    #[test]
    fn test_remove_applied_fee_bump_uses_inner_source() {
        let queue = TransactionQueue::with_defaults();

        // Queue a regular tx from inner_source (seed 50) with seq=1.
        let mut queued_tx = make_test_envelope(200, 1);
        set_source(&mut queued_tx, 50);
        assert_eq!(queue.try_add(queued_tx.clone()), TxQueueResult::Added);
        let queued_hash = full_hash(&queued_tx);
        assert!(queue.contains(&queued_hash));

        // Build a fee-bump applied tx: inner_source = seed 50 (same account),
        // fee_source = seed 99 (different account), inner seq = 5.
        let mut inner = match make_test_envelope(100, 1) {
            TransactionEnvelope::Tx(env) => env,
            _ => panic!("expected Tx"),
        };
        inner.tx.source_account = MuxedAccount::Ed25519(Uint256([50; 32]));
        inner.tx.seq_num = SequenceNumber(5);
        let applied_fee_bump = make_fee_bump_envelope(inner, 99, 500);

        // remove_applied should match by inner source (seed 50) and drop the
        // queued tx because its seq(1) <= applied seq(5).
        queue.remove_applied(&[(applied_fee_bump, 5)]);

        assert!(
            !queue.contains(&queued_hash),
            "fee-bump remove_applied should match by inner source account"
        );
        assert_eq!(queue.len(), 0);
    }

    /// Fee-bump removal should NOT match against the outer fee source.
    #[test]
    fn test_remove_applied_fee_bump_does_not_match_fee_source() {
        let queue = TransactionQueue::with_defaults();

        // Queue a tx from account seed 99 (the fee source of the fee-bump).
        let mut queued_tx = make_test_envelope(200, 1);
        set_source(&mut queued_tx, 99);
        assert_eq!(queue.try_add(queued_tx.clone()), TxQueueResult::Added);
        let queued_hash = full_hash(&queued_tx);

        // Build a fee-bump: inner_source = seed 50, fee_source = seed 99.
        let mut inner = match make_test_envelope(100, 1) {
            TransactionEnvelope::Tx(env) => env,
            _ => panic!("expected Tx"),
        };
        inner.tx.source_account = MuxedAccount::Ed25519(Uint256([50; 32]));
        inner.tx.seq_num = SequenceNumber(5);
        let applied_fee_bump = make_fee_bump_envelope(inner, 99, 500);

        queue.remove_applied(&[(applied_fee_bump, 5)]);

        // Queued tx from account 99 should NOT be removed — the fee-bump's
        // inner source is 50, not 99.
        assert!(
            queue.contains(&queued_hash),
            "fee-bump remove_applied should not match by outer fee source"
        );
        assert_eq!(queue.len(), 1);
    }

    /// AUDIT-088: Replace-by-fee must succeed when the queue is at the max_queue_ops
    /// limit, because the old tx's ops should be excluded from capacity calculations.
    /// Without the fix, the fee-bump would be rejected as QueueFull because the
    /// eviction check doesn't account for the to-be-replaced tx's resources.
    #[test]
    fn test_audit_088_replace_by_fee_at_ops_limit() {
        // max_queue_ops = 4: the fee-bump costs 3 ops (2 inner + 1 wrapper),
        // plus tx_b's 1 op = 4 total.
        let config = TxQueueConfig {
            max_queue_ops: Some(4),
            max_size: 10,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // tx_a: 2 ops from source 50, seq=1, fee=200
        let mut tx_a = make_test_envelope(200, 2);
        set_source(&mut tx_a, 50);

        // tx_b: 1 op from source 51, fee=100
        let mut tx_b = make_test_envelope(100, 1);
        set_source(&mut tx_b, 51);

        // Add both: now at 3/4 ops (tx_a=2 + tx_b=1)
        assert_eq!(queue.try_add(tx_a.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_b), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        // Build a fee-bump wrapping the same inner tx (source 50, seq 1)
        // with a higher fee. This should succeed because the old tx's 2 ops
        // are excluded from the capacity check. The fee-bump costs 3 ops
        // (2 inner + 1 fee-bump wrapper), matching stellar-core's
        // FeeBumpTransactionFrame::getNumOperations().
        let inner = match tx_a {
            TransactionEnvelope::Tx(ref env) => env.clone(),
            _ => panic!("expected v1"),
        };
        let fee_bump = make_fee_bump_envelope(inner, 60, 10000);

        let result = queue.try_add(fee_bump);
        assert_eq!(result, TxQueueResult::Added);
        // The old tx_a should be replaced, queue still has 2 entries
        assert_eq!(queue.len(), 2);
    }

    /// AUDIT-089: Evicted transactions must not be removed from the queue if
    /// a later validation step (fee-balance check) rejects the candidate.
    /// Prior to this fix, evicted txs were removed before fee validation,
    /// leaving the queue corrupted on rejection.
    #[test]
    fn test_audit_089_eviction_rollback_on_fee_rejection() {
        struct ZeroBalanceProvider;
        impl FeeBalanceProvider for ZeroBalanceProvider {
            fn get_available_balance(
                &self,
                _account_id: &AccountId,
            ) -> henyey_ledger::Result<Option<i64>> {
                Ok(Some(0)) // zero balance → candidate will be rejected
            }
        }

        let config = TxQueueConfig {
            max_queue_ops: Some(1),
            max_size: 10,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);
        queue.set_skip_fee_balance_check(false);

        // Add a low-fee victim (1 op)
        let mut victim = make_test_envelope(100, 1);
        set_source(&mut victim, 200);
        let victim_hash = full_hash(&victim);
        assert_eq!(queue.try_add(victim), TxQueueResult::Added);
        assert_eq!(queue.len(), 1);

        // Set provider with zero balance so the candidate will be rejected
        queue.set_fee_balance_provider(Arc::new(ZeroBalanceProvider));

        // Submit a higher-fee candidate that would evict the victim
        let mut candidate = make_test_envelope(1000, 1);
        set_source(&mut candidate, 201);
        let result = queue.try_add(candidate);

        // Candidate must be rejected
        assert_eq!(
            result,
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxInsufficientBalance))
        );

        // Victim must still be in the queue (not evicted)
        assert!(
            queue.contains(&victim_hash),
            "evicted victim must be restored after fee-balance rejection"
        );
        assert_eq!(queue.len(), 1);
    }

    /// pending_envelopes returns all queued transaction envelopes.
    #[test]
    fn test_pending_envelopes() {
        let queue = TransactionQueue::with_defaults();

        assert!(queue.pending_envelopes().is_empty());

        let mut tx1 = make_test_envelope(200, 1);
        set_source(&mut tx1, 10);
        let mut tx2 = make_test_envelope(300, 1);
        set_source(&mut tx2, 20);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);

        let pending = queue.pending_envelopes();
        assert_eq!(pending.len(), 2);
    }

    /// `pending_hashed_envelopes` returns correct hashes matching `Hash256::hash_xdr`.
    #[test]
    fn test_pending_hashed_envelopes_returns_correct_hashes() {
        let queue = TransactionQueue::with_defaults();

        let mut tx1 = make_test_envelope(200, 1);
        set_source(&mut tx1, 10);
        let mut tx2 = make_test_envelope(300, 1);
        set_source(&mut tx2, 20);

        let hash1 = Hash256::hash_xdr(&tx1);
        let hash2 = Hash256::hash_xdr(&tx2);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);

        let pending = queue.pending_hashed_envelopes();
        assert_eq!(pending.len(), 2);

        let returned_hashes: std::collections::HashSet<Hash256> =
            pending.iter().map(|htx| htx.hash()).collect();
        assert!(returned_hashes.contains(&hash1));
        assert!(returned_hashes.contains(&hash2));

        // Verify each hash matches hash_xdr of its envelope.
        for htx in &pending {
            assert_eq!(htx.hash(), Hash256::hash_xdr(htx.envelope()));
        }
    }

    /// Dynamic Soroban resource limits override static config.
    #[test]
    fn test_effective_queue_soroban_resources_dynamic_override() {
        let static_limit = Resource::new(vec![100; NUM_SOROBAN_TX_RESOURCES]);
        let config = TxQueueConfig {
            max_queue_soroban_resources: Some(static_limit.clone()),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Before any dynamic update, effective returns the static config value.
        let eff = queue.effective_queue_soroban_resources().unwrap();
        assert_eq!(eff, static_limit);

        // After dynamic update, the dynamic value takes precedence.
        let dynamic_limit = Resource::new(vec![999; NUM_SOROBAN_TX_RESOURCES]);
        queue.update_soroban_resource_limits(dynamic_limit.clone());
        let eff = queue.effective_queue_soroban_resources().unwrap();
        assert_eq!(eff, dynamic_limit);
    }

    /// Regression for #3612: the classic queue's effective ops/size capacity
    /// must track the live `maxTxSetSize` (scaled by POOL_LEDGER_MULTIPLIER and
    /// applied via `update_classic_queue_capacity`) on every ledger close,
    /// mirroring stellar-core `TxQueueLimiter::reset` rebuilding capacity from
    /// `maxScaledLedgerResources`. Before the fix the capacity is frozen at the
    /// construction value and ignores `UpgradeMaxTxSetSize`.
    #[test]
    fn test_classic_queue_capacity_tracks_live_max_tx_set_size() {
        // Construction-time capacity: maxTxSetSize=50, multiplier=2 → 100 ops.
        const MULT: u32 = 2;
        let initial_ops = 50 * MULT;
        let config = TxQueueConfig {
            max_size: initial_ops as usize,
            max_queue_ops: Some(initial_ops),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Before any per-ledger update, the effective capacity is the
        // construction-time value.
        assert_eq!(queue.effective_max_queue_ops(), Some(initial_ops));
        assert_eq!(queue.effective_max_size(), initial_ops as usize);

        // UpgradeMaxTxSetSize raises maxTxSetSize 50 → 200; the per-ledger-close
        // reset must grow the effective capacity to 200 * MULT = 400 ops.
        let upgraded_ops = 200 * MULT;
        queue.update_classic_queue_capacity(upgraded_ops);
        assert_eq!(queue.effective_max_queue_ops(), Some(upgraded_ops));
        assert_eq!(queue.effective_max_size(), upgraded_ops as usize);

        // A downgrade 200 → 50 must shrink the capacity symmetrically.
        let downgraded_ops = 50 * MULT;
        queue.update_classic_queue_capacity(downgraded_ops);
        assert_eq!(queue.effective_max_queue_ops(), Some(downgraded_ops));
        assert_eq!(queue.effective_max_size(), downgraded_ops as usize);
    }

    /// End-to-end admission test for #3615 (follow-up to #3612 / PR #3614).
    ///
    /// The getter-only sibling test
    /// (`test_classic_queue_capacity_tracks_live_max_tx_set_size`) asserts that
    /// `effective_max_queue_ops` / `effective_max_size` reflect the dynamic
    /// capacity, but does not drive transactions THROUGH the rewired admission
    /// gates. This test exercises the actual `try_add` admission/eviction path
    /// to confirm that:
    ///
    ///   - at the OLD (lower) cap, a tx beyond capacity is REJECTED;
    ///   - after an `UpgradeMaxTxSetSize` (applied via
    ///     `update_classic_queue_capacity`), the SAME tx is now ACCEPTED, with
    ///     no eviction — i.e. the dynamically-grown capacity is honored by the
    ///     gate, not just the getter;
    ///   - after a symmetric downgrade, admission again REJECTS above the new
    ///     lower cap.
    ///
    /// Parity: capacity = `maxTxSetSize * POOL_LEDGER_MULTIPLIER`, mirroring the
    /// production caller in `app/src/app/ledger_close.rs`, which passes
    /// `header.max_tx_set_size.saturating_mul(POOL_LEDGER_MULTIPLIER)` into
    /// `update_classic_queue_capacity` on every ledger close. Uses single-op
    /// classic txs so the op count equals the tx count, making the global-ops
    /// gate the load-bearing limiter. All admitted txs share the same fee and
    /// use distinct source accounts, so capacity — not fee-based eviction — is
    /// the variable under test (an equal-fee tx against a full queue cannot
    /// displace an incumbent, so any growth in admissions is attributable to the
    /// raised capacity alone).
    #[test]
    fn test_classic_queue_admission_respects_dynamic_capacity() {
        // POOL_LEDGER_MULTIPLIER is 2 (see #3612 wiring); mirror it here so the
        // scaled capacity matches the production caller.
        const MULT: u32 = 2;
        // maxTxSetSize = 2 → scaled cap = 4 ops. `max_size` is set high so the
        // construction-time tx-count gate never bites; the global-ops gate (the
        // one #3614 rewired to track the live cap) is the limiter. `min_fee_per_op`
        // = 0 so the fee gate is permissive — admission is governed purely by
        // capacity here.
        let initial_cap = 2 * MULT; // 4
        let config = TxQueueConfig {
            max_queue_ops: Some(initial_cap),
            max_size: 64,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Every admitted tx is a single-op classic payment with the SAME fee,
        // from a DISTINCT source account (distinct sources dodge the per-account
        // one-tx limit; equal fees mean an over-cap tx cannot evict an incumbent).
        let same_fee = 1_000u32;
        let make = |seed: u8| {
            let mut env = make_test_envelope(same_fee, 1);
            set_source(&mut env, seed);
            env
        };

        // --- Phase 1: fill to the OLD cap, then a beyond-cap tx is REJECTED. ---
        for seed in 0..initial_cap as u8 {
            assert_eq!(
                queue.try_add(make(seed + 1)),
                TxQueueResult::Added,
                "tx {} should fit within the initial cap of {} ops",
                seed,
                initial_cap
            );
        }
        assert_eq!(queue.len(), initial_cap as usize);

        // The (cap+1)-th equal-fee tx cannot be admitted: the global-ops queue is
        // full and an equal fee cannot displace an incumbent. Assert it is NOT
        // admitted and the queue did not grow past the old cap.
        let over_cap_env = make(200);
        let over_cap_hash = full_hash(&over_cap_env);
        let rejected = queue.try_add(over_cap_env);
        assert_ne!(
            rejected,
            TxQueueResult::Added,
            "admission must reject beyond the OLD cap (got {:?})",
            rejected
        );
        assert!(!queue.contains(&over_cap_hash));
        assert_eq!(queue.len(), initial_cap as usize);

        // --- Phase 2: UpgradeMaxTxSetSize raises maxTxSetSize 2 → 8; the per-
        // ledger-close reset grows the scaled cap to 8 * MULT = 16 ops. The SAME
        // over-cap tx must now be ADMITTED (capacity grew; no eviction needed). ---
        let upgraded_cap = 8 * MULT; // 16
        queue.update_classic_queue_capacity(upgraded_cap);
        assert_eq!(queue.effective_max_queue_ops(), Some(upgraded_cap));

        let upgrade_env = make(200);
        let upgrade_hash = full_hash(&upgrade_env);
        assert_eq!(
            queue.try_add(upgrade_env),
            TxQueueResult::Added,
            "after upgrade the over-old-cap tx must be admitted"
        );
        assert!(queue.contains(&upgrade_hash));
        // No incumbent was evicted: the queue grew from old-cap to old-cap+1.
        assert_eq!(queue.len(), initial_cap as usize + 1);

        // Keep filling up to the NEW cap to prove the gate honors the full raised
        // capacity, not just one extra slot.
        let mut next_seed = initial_cap as u8 + 1; // sources used so far: 1..=cap, plus 200
        while queue.len() < upgraded_cap as usize {
            assert_eq!(
                queue.try_add(make(next_seed)),
                TxQueueResult::Added,
                "tx should fit within the upgraded cap of {} ops",
                upgraded_cap
            );
            next_seed += 1;
        }
        assert_eq!(queue.len(), upgraded_cap as usize);

        // At the new cap, a further equal-fee tx is rejected again.
        let over_new_env = make(201);
        let over_new_hash = full_hash(&over_new_env);
        let rejected_at_new = queue.try_add(over_new_env);
        assert_ne!(
            rejected_at_new,
            TxQueueResult::Added,
            "admission must reject beyond the UPGRADED cap (got {:?})",
            rejected_at_new
        );
        assert!(!queue.contains(&over_new_hash));
        assert_eq!(queue.len(), upgraded_cap as usize);

        // --- Phase 3: downgrade. A fresh queue at the lower cap must reject
        // admissions above that lower cap — the symmetric shrink path. (We use a
        // fresh queue so the assertion is about admission, not about whether a
        // downgrade retroactively evicts already-queued txs, which stellar-core
        // handles lazily on the next admission/selection.) ---
        let downgraded_cap = MULT; // 1 * MULT = 2
        let config2 = TxQueueConfig {
            max_queue_ops: Some(initial_cap),
            max_size: 64,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue2 = TransactionQueue::new(config2);
        queue2.update_classic_queue_capacity(downgraded_cap);
        assert_eq!(queue2.effective_max_queue_ops(), Some(downgraded_cap));

        for seed in 0..downgraded_cap as u8 {
            assert_eq!(
                queue2.try_add(make(seed + 1)),
                TxQueueResult::Added,
                "tx {} should fit within the downgraded cap of {} ops",
                seed,
                downgraded_cap
            );
        }
        assert_eq!(queue2.len(), downgraded_cap as usize);

        let over_down_env = make(202);
        let over_down_hash = full_hash(&over_down_env);
        let rejected_down = queue2.try_add(over_down_env);
        assert_ne!(
            rejected_down,
            TxQueueResult::Added,
            "admission must reject beyond the DOWNGRADED cap (got {:?})",
            rejected_down
        );
        assert!(!queue2.contains(&over_down_hash));
        assert_eq!(queue2.len(), downgraded_cap as usize);
    }

    /// Without static config, effective returns None until dynamic update.
    #[test]
    fn test_effective_queue_soroban_resources_none_without_config() {
        let queue = TransactionQueue::with_defaults();

        // No static config and no dynamic update → None.
        assert!(queue.effective_queue_soroban_resources().is_none());

        // After dynamic update, the dynamic value is returned.
        let dynamic_limit = Resource::new(vec![500; NUM_SOROBAN_TX_RESOURCES]);
        queue.update_soroban_resource_limits(dynamic_limit.clone());
        let eff = queue.effective_queue_soroban_resources().unwrap();
        assert_eq!(eff, dynamic_limit);
    }

    /// Selection limits (1x ledger max) are separate from queue-admission limits (2x).
    #[test]
    fn test_selection_soroban_resources_separate_from_queue() {
        let queue = TransactionQueue::with_defaults();

        // Initially both are None.
        assert!(queue.effective_selection_soroban_resources().is_none());
        assert!(queue.effective_queue_soroban_resources().is_none());

        // Set queue-admission limits (2x) and selection limits (1x).
        let queue_limit = Resource::new(vec![200; NUM_SOROBAN_TX_RESOURCES]);
        let selection_limit = Resource::new(vec![100; NUM_SOROBAN_TX_RESOURCES]);
        queue.update_soroban_resource_limits(queue_limit.clone());
        queue.update_soroban_selection_limits(selection_limit.clone());

        // They should be independent.
        assert_eq!(
            queue.effective_queue_soroban_resources().unwrap(),
            queue_limit
        );
        assert_eq!(
            queue.effective_selection_soroban_resources().unwrap(),
            selection_limit
        );
    }

    /// Regression: soroban_ledger_limits() produces the canonical ResourceType ordering
    /// so that position [2] is TxByteSize (not ReadLedgerEntries). A misordering
    /// causes Soroban transactions to be rejected with QueueFull when their
    /// byte size exceeds the tiny read-entry count (e.g. 6).
    #[test]
    fn test_soroban_ledger_limits_ordering_matches_tx_resources() {
        use henyey_common::ResourceType;

        let limit = Resource::soroban_ledger_limits(
            2,         // tx_count
            5_000_000, // instructions
            20_000,    // tx_size_bytes
            6_400,     // read_bytes
            6_400,     // write_bytes
            6,         // read_ledger_entries
            4,         // write_ledger_entries
        );

        // Verify each position matches the canonical ResourceType index.
        assert_eq!(limit.get_val(ResourceType::Operations), 2);
        assert_eq!(limit.get_val(ResourceType::Instructions), 5_000_000);
        assert_eq!(limit.get_val(ResourceType::TxByteSize), 20_000);
        assert_eq!(limit.get_val(ResourceType::DiskReadBytes), 6_400);
        assert_eq!(limit.get_val(ResourceType::WriteBytes), 6_400);
        assert_eq!(limit.get_val(ResourceType::ReadLedgerEntries), 6);
        assert_eq!(limit.get_val(ResourceType::WriteLedgerEntries), 4);
    }

    /// Parity pin: the queue-size multipliers must match stellar-core
    /// `Config::TRANSACTION_QUEUE_SIZE_MULTIPLIER` /
    /// `Config::SOROBAN_TRANSACTION_QUEUE_SIZE_MULTIPLIER` (both default `2`,
    /// `src/main/Config.cpp:205-206`) and HERDER_SPEC §12.6. A change here would
    /// silently diverge queue admission capacity from upstream.
    #[test]
    fn test_transaction_queue_size_multipliers_match_stellar_core() {
        assert_eq!(TRANSACTION_QUEUE_SIZE_MULTIPLIER, 2);
        assert_eq!(SOROBAN_TRANSACTION_QUEUE_SIZE_MULTIPLIER, 2);
    }

    /// Regression: a Soroban tx whose byte size exceeds the initial min
    /// read-ledger-entries limit (6) should still be admitted when the
    /// dynamic resource limits (with correct ordering) allow it.
    #[test]
    fn test_soroban_tx_admitted_with_restrictive_initial_limits() {
        // Simulate the initial Soroban limits on a fresh protocol 25 network
        // multiplied by SOROBAN_TRANSACTION_QUEUE_SIZE_MULTIPLIER (2).
        let limit = Resource::soroban_ledger_limits(
            2,         // 1 * 2 tx_count
            5_000_000, // 2_500_000 * 2 instructions
            20_000,    // 10_000 * 2 tx_size_bytes
            6_400,     // 3_200 * 2 read_bytes
            6_400,     // 3_200 * 2 write_bytes
            6,         // 3 * 2 read_ledger_entries
            4,         // 2 * 2 write_ledger_entries
        );

        let queue = TransactionQueue::with_defaults();
        queue.update_soroban_resource_limits(limit);

        // A Soroban tx with modest resources that fit within limits.
        // The tx XDR is a few hundred bytes — within 20,000 byte limit.
        // With the old misordered resource vector, position [2] was
        // read_ledger_entries (= 6), so any tx with byte size > 6
        // was rejected as QueueFull.
        let mut tx = make_soroban_envelope(1000);
        set_source(&mut tx, 50);
        let result = queue.try_add(tx);
        assert_eq!(result, TxQueueResult::Added);
    }

    /// ban() must clean up account_states (transaction, fees, empty entries)
    /// so the account can submit new transactions after the ban expires.
    #[test]
    fn test_ban_clears_account_state() {
        let queue = TransactionQueue::with_defaults();

        let mut tx = make_test_envelope(200, 1);
        set_source(&mut tx, 70);
        let hash = full_hash(&tx);

        assert_eq!(queue.try_add(tx.clone()), TxQueueResult::Added);
        assert!(queue.account_states.read().contains_key(&account_key(&tx)));

        queue.ban(&[hash]);

        // Transaction should be removed from queue
        assert!(!queue.contains(&hash));
        // Account state should be fully cleaned up
        assert!(
            !queue.account_states.read().contains_key(&account_key(&tx)),
            "ban() should clean up account_states"
        );
    }

    /// clear() must also clear account_states.
    #[test]
    fn test_clear_clears_account_states() {
        let queue = TransactionQueue::with_defaults();

        let mut tx = make_test_envelope(200, 1);
        set_source(&mut tx, 80);

        assert_eq!(queue.try_add(tx), TxQueueResult::Added);
        assert!(!queue.account_states.read().is_empty());

        queue.clear();

        assert_eq!(queue.len(), 0);
        assert!(
            queue.account_states.read().is_empty(),
            "clear() should also clear account_states"
        );
    }

    #[test]
    fn test_extra_signer_required_missing() {
        let queue = TransactionQueue::with_defaults();
        let network_id = NetworkId::testnet();

        let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let extra_secret = SecretKey::from_seed(&[9u8; 32]);
        let extra_signer = SignerKey::Ed25519(Uint256(*extra_secret.public_key().as_bytes()));

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: None,
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![extra_signer].try_into().unwrap(),
        });

        let operations: Vec<Operation> = vec![Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
                starting_balance: 1000000000,
            }),
        }];

        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: preconditions,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });

        let wrong_secret = SecretKey::from_seed(&[8u8; 32]);
        let sig = sign_envelope(&envelope, &wrong_secret, &network_id);
        if let TransactionEnvelope::Tx(ref mut env) = envelope {
            env.signatures = vec![sig].try_into().unwrap();
        }

        assert!(matches!(queue.try_add(envelope), TxQueueResult::Invalid(_)));
    }

    #[test]
    fn test_extra_signer_required_satisfied() {
        let queue = TransactionQueue::with_defaults();
        let network_id = NetworkId::testnet();

        let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let extra_secret = SecretKey::from_seed(&[9u8; 32]);
        let extra_signer = SignerKey::Ed25519(Uint256(*extra_secret.public_key().as_bytes()));

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: None,
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![extra_signer].try_into().unwrap(),
        });

        let operations: Vec<Operation> = vec![Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
                starting_balance: 1000000000,
            }),
        }];

        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: preconditions,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });

        let sig = sign_envelope(&envelope, &extra_secret, &network_id);
        if let TransactionEnvelope::Tx(ref mut env) = envelope {
            env.signatures = vec![sig].try_into().unwrap();
        }

        assert_eq!(queue.try_add(envelope), TxQueueResult::Added);
    }

    #[test]
    fn test_extra_signer_signed_payload_satisfied() {
        use stellar_xdr::SignerKeyEd25519SignedPayload;

        let queue = TransactionQueue::with_defaults();

        let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let payload_secret = SecretKey::from_seed(&[10u8; 32]);
        let pubkey_bytes = *payload_secret.public_key().as_bytes();
        let payload_bytes = b"CAP-0040 test payload with enough bytes";

        let signed_payload_signer = SignerKeyEd25519SignedPayload {
            ed25519: Uint256(pubkey_bytes),
            payload: payload_bytes.to_vec().try_into().unwrap(),
        };
        let extra_signer = SignerKey::Ed25519SignedPayload(signed_payload_signer.clone());

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: None,
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![extra_signer].try_into().unwrap(),
        });

        let operations: Vec<Operation> = vec![Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
                starting_balance: 1000000000,
            }),
        }];

        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: preconditions,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        // Sign the payload bytes with the correct key
        let sig = payload_secret.sign(payload_bytes);

        // Compute CAP-0040 XOR hint: pubkey_last4 XOR payload_last4
        let pubkey_hint = [
            pubkey_bytes[28],
            pubkey_bytes[29],
            pubkey_bytes[30],
            pubkey_bytes[31],
        ];
        let plen = payload_bytes.len();
        let payload_hint = [
            payload_bytes[plen - 4],
            payload_bytes[plen - 3],
            payload_bytes[plen - 2],
            payload_bytes[plen - 1],
        ];
        let xor_hint = [
            pubkey_hint[0] ^ payload_hint[0],
            pubkey_hint[1] ^ payload_hint[1],
            pubkey_hint[2] ^ payload_hint[2],
            pubkey_hint[3] ^ payload_hint[3],
        ];

        let decorated_sig = DecoratedSignature {
            hint: SignatureHint(xor_hint),
            signature: XdrSignature(sig.0.to_vec().try_into().unwrap()),
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![decorated_sig].try_into().unwrap(),
        });

        assert_eq!(queue.try_add(envelope), TxQueueResult::Added);
    }

    #[test]
    fn test_extra_signer_signed_payload_wrong_key_rejected() {
        use henyey_tx::TxResultCode;
        use stellar_xdr::SignerKeyEd25519SignedPayload;

        let queue = TransactionQueue::with_defaults();

        let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let signer_secret = SecretKey::from_seed(&[10u8; 32]);
        let wrong_secret = SecretKey::from_seed(&[11u8; 32]);
        let signer_pubkey_bytes = *signer_secret.public_key().as_bytes();
        let payload_bytes = b"CAP-0040 test payload with enough bytes";

        // The extra signer references signer_secret's public key
        let signed_payload_signer = SignerKeyEd25519SignedPayload {
            ed25519: Uint256(signer_pubkey_bytes),
            payload: payload_bytes.to_vec().try_into().unwrap(),
        };
        let extra_signer = SignerKey::Ed25519SignedPayload(signed_payload_signer);

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: None,
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![extra_signer].try_into().unwrap(),
        });

        let operations: Vec<Operation> = vec![Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
                starting_balance: 1000000000,
            }),
        }];

        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: preconditions,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        // Sign with wrong key — hint will also be wrong (derived from wrong key)
        let wrong_pubkey_bytes = *wrong_secret.public_key().as_bytes();
        let sig = wrong_secret.sign(payload_bytes);
        let wrong_hint = [
            wrong_pubkey_bytes[28],
            wrong_pubkey_bytes[29],
            wrong_pubkey_bytes[30],
            wrong_pubkey_bytes[31],
        ];
        let plen = payload_bytes.len();
        let payload_hint = [
            payload_bytes[plen - 4],
            payload_bytes[plen - 3],
            payload_bytes[plen - 2],
            payload_bytes[plen - 1],
        ];
        let xor_hint = [
            wrong_hint[0] ^ payload_hint[0],
            wrong_hint[1] ^ payload_hint[1],
            wrong_hint[2] ^ payload_hint[2],
            wrong_hint[3] ^ payload_hint[3],
        ];

        let decorated_sig = DecoratedSignature {
            hint: SignatureHint(xor_hint),
            signature: XdrSignature(sig.0.to_vec().try_into().unwrap()),
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![decorated_sig].try_into().unwrap(),
        });

        assert_eq!(
            queue.try_add(envelope),
            TxQueueResult::Invalid(Some(TxResultCode::TxBadAuth))
        );
    }

    #[test]
    fn test_extra_signer_signed_payload_bad_signature_rejected() {
        use henyey_tx::TxResultCode;
        use stellar_xdr::SignerKeyEd25519SignedPayload;

        let queue = TransactionQueue::with_defaults();

        let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let payload_secret = SecretKey::from_seed(&[10u8; 32]);
        let pubkey_bytes = *payload_secret.public_key().as_bytes();
        let payload_bytes = b"CAP-0040 test payload with enough bytes";

        let signed_payload_signer = SignerKeyEd25519SignedPayload {
            ed25519: Uint256(pubkey_bytes),
            payload: payload_bytes.to_vec().try_into().unwrap(),
        };
        let extra_signer = SignerKey::Ed25519SignedPayload(signed_payload_signer);

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: None,
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![extra_signer].try_into().unwrap(),
        });

        let operations: Vec<Operation> = vec![Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
                starting_balance: 1000000000,
            }),
        }];

        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: preconditions,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        // Compute correct XOR hint (passes hint check)
        let pubkey_hint = [
            pubkey_bytes[28],
            pubkey_bytes[29],
            pubkey_bytes[30],
            pubkey_bytes[31],
        ];
        let plen = payload_bytes.len();
        let payload_hint = [
            payload_bytes[plen - 4],
            payload_bytes[plen - 3],
            payload_bytes[plen - 2],
            payload_bytes[plen - 1],
        ];
        let xor_hint = [
            pubkey_hint[0] ^ payload_hint[0],
            pubkey_hint[1] ^ payload_hint[1],
            pubkey_hint[2] ^ payload_hint[2],
            pubkey_hint[3] ^ payload_hint[3],
        ];

        // Create a valid-length but corrupted signature (64 bytes of garbage)
        let mut bad_sig_bytes = [0xAA_u8; 64];
        bad_sig_bytes[0] = 0xFF;

        let decorated_sig = DecoratedSignature {
            hint: SignatureHint(xor_hint),
            signature: XdrSignature(bad_sig_bytes.to_vec().try_into().unwrap()),
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![decorated_sig].try_into().unwrap(),
        });

        assert_eq!(
            queue.try_add(envelope),
            TxQueueResult::Invalid(Some(TxResultCode::TxBadAuth))
        );
    }

    #[test]
    fn test_extra_signer_signed_payload_empty_payload_rejected() {
        use henyey_tx::TxResultCode;
        use stellar_xdr::SignerKeyEd25519SignedPayload;

        let queue = TransactionQueue::with_defaults();

        let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let payload_secret = SecretKey::from_seed(&[10u8; 32]);
        let pubkey_bytes = *payload_secret.public_key().as_bytes();

        // Empty payload — should be rejected structurally by check_valid_pre_seq_num_with_config
        let signed_payload_signer = SignerKeyEd25519SignedPayload {
            ed25519: Uint256(pubkey_bytes),
            payload: vec![].try_into().unwrap(),
        };
        let extra_signer = SignerKey::Ed25519SignedPayload(signed_payload_signer);

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: None,
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![extra_signer].try_into().unwrap(),
        });

        let operations: Vec<Operation> = vec![Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
                starting_balance: 1000000000,
            }),
        }];

        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: preconditions,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });

        assert_eq!(
            queue.try_add(envelope),
            TxQueueResult::Invalid(Some(TxResultCode::TxMalformed))
        );
    }

    #[test]
    fn test_min_seq_age_allowed() {
        let queue = TransactionQueue::with_defaults();
        let network_id = NetworkId::testnet();
        let source_secret = SecretKey::from_seed(&[5u8; 32]);
        let source = MuxedAccount::Ed25519(Uint256(*source_secret.public_key().as_bytes()));
        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: None,
            min_seq_num: None,
            min_seq_age: Duration(1),
            min_seq_ledger_gap: 0,
            extra_signers: VecM::default(),
        });

        let operation = Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
                starting_balance: 1000000000,
            }),
        };

        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: preconditions,
            memo: Memo::None,
            operations: vec![operation].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });

        let mut signed = envelope;
        let sig = sign_envelope(&signed, &source_secret, &network_id);
        if let TransactionEnvelope::Tx(ref mut env) = signed {
            env.signatures = vec![sig].try_into().unwrap();
        }

        assert_eq!(queue.try_add(signed), TxQueueResult::Added);
    }

    /// Regression test for AUDIT-093: queue admission must reject transactions
    /// whose max_time will expire before the estimated next ledger close.
    /// stellar-core uses getUpperBoundCloseTimeOffset (= expected_close_time * 2 + drift)
    /// to catch these; Henyey was only checking against the stale lcl_close_time.
    #[test]
    fn test_audit_093_queue_rejects_expiring_tx() {
        use henyey_tx::TxResultCode;
        use stellar_xdr::TimeBounds;

        let lcl_close_time: u64 = 1_700_000_000;
        let expected_close_secs: u64 = 5;
        // Upper bound offset = expected_close_time * 2 + drift.
        // With drift=0 (just closed), offset = 10.
        // A tx with max_time = lcl_close_time + 3 would expire before
        // lcl_close_time + 10, so it should be rejected.
        let max_time = lcl_close_time + 3;

        let config = TxQueueConfig {
            validate_signatures: false,
            validate_bounds: true,
            expected_ledger_close_secs: expected_close_secs,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);
        queue.update_validation_context(
            100,
            lcl_close_time,
            21,
            100,
            5_000_000,
            0,
            std::time::Duration::from_secs(5),
        );

        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let operations: Vec<Operation> = vec![Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([255u8; 32]))),
                starting_balance: 1000000000,
            }),
        }];

        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: Preconditions::Time(TimeBounds {
                min_time: stellar_xdr::TimePoint(0),
                max_time: stellar_xdr::TimePoint(max_time),
            }),
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        // This tx should be rejected as TxTooLate because max_time < lcl_close_time + upper_bound_offset.
        assert!(
            matches!(
                queue.try_add(envelope),
                TxQueueResult::Invalid(Some(TxResultCode::TxTooLate))
            ),
            "tx with max_time expiring before next close should be TxTooLate"
        );
    }

    /// Verify that a transaction whose max_time has already passed (before LCL
    /// close time) is rejected with TxTooLate, not TxTooEarly.
    #[test]
    fn test_queue_already_expired_tx_returns_too_late() {
        use henyey_tx::TxResultCode;
        use stellar_xdr::TimeBounds;

        let lcl_close_time: u64 = 1_700_000_000;
        let config = TxQueueConfig {
            validate_signatures: false,
            validate_bounds: true,
            expected_ledger_close_secs: 5,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);
        queue.update_validation_context(
            100,
            lcl_close_time,
            21,
            100,
            5_000_000,
            0,
            std::time::Duration::from_secs(5),
        );

        // max_time is before lcl_close_time — already expired.
        let max_time = lcl_close_time - 1;

        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let operations: Vec<Operation> = vec![Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([255u8; 32]))),
                starting_balance: 1000000000,
            }),
        }];

        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: Preconditions::Time(TimeBounds {
                min_time: stellar_xdr::TimePoint(0),
                max_time: stellar_xdr::TimePoint(max_time),
            }),
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        assert!(
            matches!(
                queue.try_add(envelope),
                TxQueueResult::Invalid(Some(TxResultCode::TxTooLate))
            ),
            "already-expired tx should be rejected with TxTooLate, not TxTooEarly"
        );
    }

    /// Verify that the tx queue uses expected_close_time from
    /// ValidationContext (not the static config) for upper-bound validation.
    #[test]
    fn test_queue_uses_dynamic_expected_close_time() {
        use henyey_tx::TxResultCode;
        use stellar_xdr::TimeBounds;

        // Use current wall-clock time so drift ≈ 0.
        let lcl_close_time: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // With 4s dynamic close time, upper_offset = 4*2 + ~0 = 8.
        // A tx with max_time = lcl + 9 should pass the upper-bound check.
        // With old static 5s, upper_offset = 5*2 = 10, and 9 < 10 → TxTooLate.
        let max_time = lcl_close_time + 9;

        let config = TxQueueConfig {
            validate_signatures: false,
            validate_bounds: true,
            expected_ledger_close_secs: 5, // Static config says 5s
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);
        // Set dynamic close time to 4s via ValidationContext
        queue.update_validation_context(
            100,
            lcl_close_time,
            21,
            100,
            5_000_000,
            0,
            std::time::Duration::from_millis(4000),
        );

        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let operations: Vec<Operation> = vec![Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([255u8; 32]))),
                starting_balance: 1000000000,
            }),
        }];

        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: Preconditions::Time(TimeBounds {
                min_time: stellar_xdr::TimePoint(0),
                max_time: stellar_xdr::TimePoint(max_time),
            }),
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        // With 4s dynamic close time, upper_offset = 4*2 + ~0 = 8.
        // max_time = lcl + 9 > lcl + 8, so NOT TxTooLate.
        let result = queue.try_add(envelope);
        assert!(
            !matches!(
                result,
                TxQueueResult::Invalid(Some(TxResultCode::TxTooLate))
            ),
            "tx with max_time beyond dynamic upper bound should NOT be TxTooLate; got {:?}",
            result
        );
    }

    #[test]
    fn test_is_filtered_empty_config() {
        let config = TxQueueConfig {
            validate_signatures: false,
            validate_bounds: false,
            filtered_operation_types: HashSet::new(), // No filters
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Create a transaction with CreateAccount operation
        let envelope = make_test_envelope(1000, 1);

        // Should NOT be filtered when no types are configured
        assert!(!queue.is_filtered(&envelope));
    }

    #[test]
    fn test_is_filtered_matching_type() {
        let mut filtered = HashSet::new();
        filtered.insert(OperationType::CreateAccount);

        let config = TxQueueConfig {
            validate_signatures: false,
            validate_bounds: false,
            filtered_operation_types: filtered,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Create a transaction with CreateAccount operation
        let envelope = make_test_envelope(1000, 1);

        // Should be filtered
        assert!(queue.is_filtered(&envelope));
    }

    #[test]
    fn test_is_filtered_non_matching_type() {
        let mut filtered = HashSet::new();
        filtered.insert(OperationType::Payment); // Filter payments, not CreateAccount

        let config = TxQueueConfig {
            validate_signatures: false,
            validate_bounds: false,
            filtered_operation_types: filtered,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Create a transaction with CreateAccount operation
        let envelope = make_test_envelope(1000, 1);

        // Should NOT be filtered (we filter Payment, not CreateAccount)
        assert!(!queue.is_filtered(&envelope));
    }

    #[test]
    fn test_try_add_filtered_transaction() {
        let mut filtered = HashSet::new();
        filtered.insert(OperationType::CreateAccount);

        let config = TxQueueConfig {
            validate_signatures: false,
            validate_bounds: false,
            filtered_operation_types: filtered,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Create a transaction with CreateAccount operation
        let envelope = make_test_envelope(1000, 1);

        // Should return Filtered result
        assert_eq!(queue.try_add(envelope), TxQueueResult::Filtered);
    }

    /// Regression: a tx with a sequence number lower than the account's
    /// pending tx must be rejected with a specific `txBAD_SEQ` code rather
    /// than `Invalid(None)` (which maps to `txINTERNAL_ERROR` over the
    /// compat HTTP API and is treated as a fatal server fault by clients
    /// like friendbot and stellar-rpc).
    #[test]
    fn test_try_add_lower_seq_returns_bad_seq_not_invalid_none() {
        let config = TxQueueConfig {
            validate_signatures: false,
            validate_bounds: false,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx_a = make_test_envelope(200, 1);
        let mut tx_b = make_test_envelope(200, 1);
        // Same source account, seq_a > seq_b.
        if let TransactionEnvelope::Tx(env) = &mut tx_a {
            env.tx.seq_num = SequenceNumber(10);
        }
        if let TransactionEnvelope::Tx(env) = &mut tx_b {
            env.tx.seq_num = SequenceNumber(5);
        }

        assert_eq!(queue.try_add(tx_a), TxQueueResult::Added);
        // The second tx with a lower seq must surface txBAD_SEQ, not
        // Invalid(None).
        assert_eq!(
            queue.try_add(tx_b),
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxBadSeq))
        );
    }

    #[test]
    fn test_is_filtered_soroban_type() {
        let mut filtered = HashSet::new();
        filtered.insert(OperationType::InvokeHostFunction);

        let config = TxQueueConfig {
            validate_signatures: false,
            validate_bounds: false,
            filtered_operation_types: filtered,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Create a Soroban transaction
        let envelope = make_soroban_envelope(1000);

        // Should be filtered
        assert!(queue.is_filtered(&envelope));
    }

    #[test]
    fn test_is_filtered_multiple_ops_one_filtered() {
        let mut filtered = HashSet::new();
        // Filter ManageSellOffer operations
        filtered.insert(OperationType::ManageSellOffer);

        let config = TxQueueConfig {
            validate_signatures: false,
            validate_bounds: false,
            filtered_operation_types: filtered,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Create a transaction with multiple operations - CreateAccount and ManageSellOffer
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let operations = vec![
            Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                    starting_balance: 1000000000,
                }),
            },
            Operation {
                source_account: None,
                body: OperationBody::ManageSellOffer(ManageSellOfferOp {
                    selling: Asset::Native,
                    buying: Asset::CreditAlphanum4(AlphaNum4 {
                        asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                        issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
                    }),
                    amount: 1000,
                    price: Price { n: 1, d: 1 },
                    offer_id: 0,
                }),
            },
        ];

        let tx = Transaction {
            source_account: source,
            fee: 1000,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        // Should be filtered because one operation is ManageSellOffer
        assert!(queue.is_filtered(&envelope));
    }

    // ---------------------------------------------------------------
    // Tests for reset_and_rebuild
    // ---------------------------------------------------------------

    #[test]
    fn test_reset_and_rebuild_empty_queue() {
        let queue = TransactionQueue::with_defaults();

        // Rebuild on empty queue should be a no-op
        let re_added = queue.reset_and_rebuild();
        assert_eq!(re_added, 0);
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn test_reset_and_rebuild_preserves_valid_transactions() {
        let queue = TransactionQueue::with_defaults();

        // Add several transactions with different source accounts
        let mut tx1 = make_test_envelope(200, 1);
        set_source(&mut tx1, 1);
        let mut tx2 = make_test_envelope(300, 1);
        set_source(&mut tx2, 2);
        let mut tx3 = make_test_envelope(400, 1);
        set_source(&mut tx3, 3);

        assert_eq!(queue.try_add(tx1.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx2.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx3.clone()), TxQueueResult::Added);
        assert_eq!(queue.len(), 3);

        // Rebuild should re-add all valid transactions
        let re_added = queue.reset_and_rebuild();
        assert_eq!(re_added, 3);
        assert_eq!(queue.len(), 3);

        // Verify the same transactions are in the queue
        let hash1 = full_hash(&tx1);
        let hash2 = full_hash(&tx2);
        let hash3 = full_hash(&tx3);
        assert!(queue.contains(&hash1));
        assert!(queue.contains(&hash2));
        assert!(queue.contains(&hash3));
    }

    #[test]
    fn test_reset_and_rebuild_preserves_bans() {
        let queue = TransactionQueue::with_ban_depth(TxQueueConfig::default(), 5);

        // Add a transaction, then ban it
        let mut tx_banned = make_test_envelope(200, 1);
        set_source(&mut tx_banned, 10);
        let banned_hash = full_hash(&tx_banned);
        queue.ban(&[banned_hash]);
        assert!(queue.is_banned(&banned_hash));

        // Add another transaction that stays in the queue
        let mut tx_valid = make_test_envelope(300, 1);
        set_source(&mut tx_valid, 11);
        assert_eq!(queue.try_add(tx_valid.clone()), TxQueueResult::Added);
        assert_eq!(queue.len(), 1);

        // Rebuild should preserve bans
        let re_added = queue.reset_and_rebuild();
        assert_eq!(re_added, 1);
        assert!(queue.is_banned(&banned_hash));

        // The banned transaction should still be rejected
        assert_eq!(queue.try_add(tx_banned), TxQueueResult::Banned);
    }

    #[test]
    fn test_reset_and_rebuild_drops_txs_exceeding_new_limits() {
        // Create a queue with a max_size of 2
        let config = TxQueueConfig {
            max_size: 2,
            validate_signatures: false,
            validate_bounds: false,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Add 2 transactions (filling the queue)
        let mut tx1 = make_test_envelope(200, 1);
        set_source(&mut tx1, 1);
        let mut tx2 = make_test_envelope(300, 1);
        set_source(&mut tx2, 2);

        assert_eq!(queue.try_add(tx1.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx2.clone()), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        // Rebuild should re-add all transactions since they still fit
        let re_added = queue.reset_and_rebuild();
        assert_eq!(re_added, 2);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn test_reset_and_rebuild_clears_eviction_thresholds() {
        let config = TxQueueConfig {
            max_queue_soroban_resources: Some(Resource::new(vec![10])),
            validate_signatures: false,
            validate_bounds: false,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Add and evict to set eviction thresholds
        let mut tx1 = make_test_envelope(200, 1);
        set_source(&mut tx1, 1);
        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);

        // After rebuild, eviction thresholds should be reset
        queue.reset_and_rebuild();

        // Verify the queue is still functional by adding a new transaction
        let mut tx_new = make_test_envelope(100, 1);
        set_source(&mut tx_new, 20);
        // Even a low-fee tx should be accepted since thresholds were cleared
        let result = queue.try_add(tx_new);
        assert_eq!(result, TxQueueResult::Added);
    }

    #[test]
    fn test_reset_and_rebuild_clears_account_states() {
        let queue = TransactionQueue::with_defaults();

        // Add a transaction
        let mut tx1 = make_test_envelope(200, 1);
        set_source(&mut tx1, 1);
        assert_eq!(queue.try_add(tx1.clone()), TxQueueResult::Added);

        // Verify account state exists
        {
            let states = queue.account_states.read();
            assert!(!states.is_empty());
        }

        // After rebuild, account states should be repopulated (not stale)
        let re_added = queue.reset_and_rebuild();
        assert_eq!(re_added, 1);

        // Account state should still exist (repopulated by try_add during rebuild)
        {
            let states = queue.account_states.read();
            assert!(!states.is_empty());
        }
    }

    #[test]
    fn test_reset_and_rebuild_allows_new_transactions_after() {
        let queue = TransactionQueue::with_defaults();

        // Add initial transactions
        let mut tx1 = make_test_envelope(200, 1);
        set_source(&mut tx1, 1);
        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);

        // Rebuild
        queue.reset_and_rebuild();

        // Should be able to add new transactions after rebuild
        let mut tx_new = make_test_envelope(400, 1);
        set_source(&mut tx_new, 50);
        assert_eq!(queue.try_add(tx_new), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn test_reset_and_rebuild_does_not_readd_same_tx_twice() {
        let queue = TransactionQueue::with_defaults();

        let mut tx1 = make_test_envelope(200, 1);
        set_source(&mut tx1, 1);
        let hash1 = full_hash(&tx1);
        assert_eq!(queue.try_add(tx1.clone()), TxQueueResult::Added);

        // Rebuild
        let re_added = queue.reset_and_rebuild();
        assert_eq!(re_added, 1);

        // The transaction should be in the queue exactly once
        assert_eq!(queue.len(), 1);
        assert!(queue.contains(&hash1));

        // Trying to add the same tx again should be duplicate
        assert_eq!(queue.try_add(tx1), TxQueueResult::Duplicate);
    }

    // --- P1-2: Specific error codes from TxQueueResult::Invalid ---

    #[test]
    fn test_invalid_structure_returns_tx_malformed() {
        let queue = TransactionQueue::with_defaults();

        // Zero-fee transaction should fail is_valid_structure()
        let mut envelope = make_test_envelope(0, 1);
        set_source(&mut envelope, 100);

        match queue.try_add(envelope) {
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxMalformed)) => {}
            other => panic!("expected Invalid(TxMalformed), got {:?}", other),
        }
    }

    #[test]
    fn test_zero_operations_returns_tx_malformed() {
        let queue = TransactionQueue::with_defaults();

        // Create a transaction with zero operations (violates structure check)
        let source = MuxedAccount::Ed25519(Uint256([101u8; 32]));
        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        // stellar-core returns txMISSING_OPERATION for zero-op transactions
        match queue.try_add(envelope) {
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxMissingOperation)) => {}
            other => panic!("expected Invalid(TxMissingOperation), got {:?}", other),
        }
    }

    // --- P1-1: Operation-level validation at queue time ---

    #[test]
    fn test_invalid_operation_rejected_at_queue_time() {
        let queue = TransactionQueue::with_defaults();

        // Create a transaction with an invalid payment (amount <= 0)
        let source = MuxedAccount::Ed25519(Uint256([102u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([103u8; 32]));

        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::Payment(PaymentOp {
                    destination: dest,
                    asset: Asset::Native,
                    amount: 0, // Invalid: amount must be > 0
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        match queue.try_add(envelope) {
            // Issue #2063: classic op structural errors now produce TxFailed
            // (not TxMalformed) to match stellar-core parity
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxFailed)) => {}
            other => panic!("expected Invalid(TxFailed), got {:?}", other),
        }
    }

    #[test]
    fn test_valid_operation_accepted_at_queue_time() {
        let queue = TransactionQueue::with_defaults();

        // Normal valid transaction should pass operation validation
        let mut envelope = make_test_envelope(200, 1);
        set_source(&mut envelope, 104);

        assert_eq!(queue.try_add(envelope), TxQueueResult::Added);
    }

    #[test]
    fn test_negative_payment_amount_rejected() {
        let queue = TransactionQueue::with_defaults();

        let source = MuxedAccount::Ed25519(Uint256([105u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([106u8; 32]));

        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::Payment(PaymentOp {
                    destination: dest,
                    asset: Asset::Native,
                    amount: -100, // Invalid: negative amount
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        match queue.try_add(envelope) {
            // Issue #2063: classic op structural errors now produce TxFailed
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxFailed)) => {}
            other => panic!("expected Invalid(TxFailed), got {:?}", other),
        }
    }

    // --- P1-3: Soroban memo validation at queue time ---

    #[test]
    fn test_soroban_with_memo_rejected() {
        let queue = TransactionQueue::with_defaults();

        let mut envelope = make_soroban_envelope(500);
        if let TransactionEnvelope::Tx(ref mut env) = envelope {
            env.tx.memo = Memo::Text(StringM::try_from("bad").unwrap());
        }
        set_source(&mut envelope, 107);

        match queue.try_add(envelope) {
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxSorobanInvalid)) => {}
            other => panic!("expected Invalid(TxSorobanInvalid), got {:?}", other),
        }
    }

    #[test]
    fn test_soroban_with_muxed_source_rejected() {
        let queue = TransactionQueue::with_defaults();

        let mut envelope = make_soroban_envelope(500);
        if let TransactionEnvelope::Tx(ref mut env) = envelope {
            env.tx.source_account = MuxedAccount::MuxedEd25519(MuxedAccountMed25519 {
                id: 42,
                ed25519: Uint256([108u8; 32]),
            });
        }

        match queue.try_add(envelope) {
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxSorobanInvalid)) => {}
            other => panic!("expected Invalid(TxSorobanInvalid), got {:?}", other),
        }
    }

    #[test]
    fn test_soroban_with_muxed_op_source_rejected() {
        let queue = TransactionQueue::with_defaults();

        let mut envelope = make_soroban_envelope(500);
        if let TransactionEnvelope::Tx(ref mut env) = envelope {
            let mut ops: Vec<Operation> = env.tx.operations.to_vec();
            ops[0].source_account = Some(MuxedAccount::MuxedEd25519(MuxedAccountMed25519 {
                id: 99,
                ed25519: Uint256([109u8; 32]),
            }));
            env.tx.operations = ops.try_into().unwrap();
        }
        set_source(&mut envelope, 110);

        match queue.try_add(envelope) {
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxSorobanInvalid)) => {}
            other => panic!("expected Invalid(TxSorobanInvalid), got {:?}", other),
        }
    }

    #[test]
    fn test_soroban_without_memo_accepted() {
        let queue = TransactionQueue::with_defaults();

        // Normal soroban tx with MEMO_NONE should pass memo validation
        let mut envelope = make_soroban_envelope(500);
        set_source(&mut envelope, 111);

        assert_eq!(queue.try_add(envelope), TxQueueResult::Added);
    }

    // --- P3: Soroban create-contract host function pairing validation at queue time ---

    /// Helper to create a Soroban envelope with a CreateContract host function.
    fn make_create_contract_envelope(
        preimage: ContractIdPreimage,
        executable: ContractExecutable,
    ) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([120u8; 32]));
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::CreateContract(CreateContractArgs {
                    contract_id_preimage: preimage,
                    executable,
                }),
                auth: VecM::default(),
            }),
        };
        let tx = Transaction {
            source_account: source,
            fee: 500,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V1(SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: LedgerFootprint {
                        read_only: VecM::default(),
                        read_write: VecM::default(),
                    },
                    instructions: 100,
                    disk_read_bytes: 0,
                    write_bytes: 0,
                },
                resource_fee: 50,
            }),
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    #[test]
    fn test_create_contract_from_asset_with_wasm_rejected() {
        let queue = TransactionQueue::with_defaults();
        let mut envelope = make_create_contract_envelope(
            ContractIdPreimage::Asset(Asset::Native),
            ContractExecutable::Wasm(Hash([12u8; 32])),
        );
        set_source(&mut envelope, 121);

        match queue.try_add(envelope) {
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxSorobanInvalid)) => {}
            other => panic!("expected Invalid(TxSorobanInvalid), got {:?}", other),
        }
    }

    #[test]
    fn test_create_contract_from_address_with_stellar_asset_rejected() {
        let queue = TransactionQueue::with_defaults();
        let mut envelope = make_create_contract_envelope(
            ContractIdPreimage::Address(ContractIdPreimageFromAddress {
                address: ScAddress::Account(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
                    [10u8; 32],
                )))),
                salt: Uint256([11u8; 32]),
            }),
            ContractExecutable::StellarAsset,
        );
        set_source(&mut envelope, 122);

        match queue.try_add(envelope) {
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxSorobanInvalid)) => {}
            other => panic!("expected Invalid(TxSorobanInvalid), got {:?}", other),
        }
    }

    #[test]
    fn test_create_contract_valid_pairing_accepted() {
        let queue = TransactionQueue::with_defaults();
        let mut envelope = make_create_contract_envelope(
            ContractIdPreimage::Asset(Asset::Native),
            ContractExecutable::StellarAsset,
        );
        set_source(&mut envelope, 123);

        assert_eq!(queue.try_add(envelope), TxQueueResult::Added);
    }

    /// Soroban transaction with resource_fee > total_fee is rejected as TxSorobanInvalid.
    /// Regression test for AUDIT-H19.
    #[test]
    fn test_soroban_resource_fee_exceeds_total_fee_rejected() {
        let queue = TransactionQueue::with_defaults();
        // Create a soroban tx with fee=200 and resource_fee=500 (exceeds total)
        let mut envelope = make_soroban_envelope_with_resources(200, 100);
        if let TransactionEnvelope::Tx(ref mut env) = envelope {
            if let TransactionExt::V1(ref mut data) = env.tx.ext {
                data.resource_fee = 500; // > total_fee (200)
            }
        }
        set_source(&mut envelope, 130);

        match queue.try_add(envelope) {
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxSorobanInvalid)) => {}
            other => panic!("expected Invalid(TxSorobanInvalid), got {:?}", other),
        }
    }

    /// Soroban transaction with resource_fee == total_fee has
    /// `inclusion_fee == 0` and is rejected by the inclusion-fee
    /// admission gate.
    ///
    /// Parity: stellar-core `TransactionFrame::commonValid` chargeFee path
    /// at `TransactionFrame.cpp:1482-1487` rejects with
    /// `txINSUFFICIENT_FEE` when `getInclusionFee() < getMinInclusionFee`.
    ///
    /// Regression test for AUDIT-214 (#2103). Previously henyey accepted
    /// such transactions because admission compared `total_fee / op_count`
    /// against `base_fee` rather than inclusion fee against
    /// `min_inclusion_fee`, causing the herder to nominate tx sets that
    /// failed its own `check_fee_map` and were rejected by stellar-core
    /// peers.
    #[test]
    fn test_soroban_zero_inclusion_fee_rejected() {
        let queue = TransactionQueue::with_defaults();
        // fee=500, resource_fee=500 → inclusion_fee=0 < min=100
        // (base_fee=100 from with_defaults, op_count=1).
        let envelope = make_soroban_envelope_with_resource_fee(500, 500, 100);
        assert_eq!(queue.try_add(envelope), TxQueueResult::FeeTooLow);
    }

    /// Soroban tx with `inclusion_fee < base_fee * op_count` is rejected.
    /// Regression test for AUDIT-214 (#2103).
    #[test]
    fn test_soroban_low_inclusion_fee_rejected() {
        let queue = TransactionQueue::with_defaults();
        // fee=550, resource_fee=500 → inclusion_fee=50 < min=100
        let envelope = make_soroban_envelope_with_resource_fee(550, 500, 100);
        assert_eq!(queue.try_add(envelope), TxQueueResult::FeeTooLow);
    }

    /// Soroban tx with `inclusion_fee == base_fee * op_count` is accepted
    /// (boundary case for AUDIT-214 (#2103)).
    #[test]
    fn test_soroban_inclusion_fee_at_minimum_accepted() {
        let queue = TransactionQueue::with_defaults();
        // fee=600, resource_fee=500 → inclusion_fee=100 == min=100
        let envelope = make_soroban_envelope_with_resource_fee(600, 500, 100);
        assert_eq!(queue.try_add(envelope), TxQueueResult::Added);
    }

    /// Fee-bump wrapping a Soroban inner tx with zero outer inclusion fee
    /// is rejected. Parity:
    /// `FeeBumpTransactionFrame::getInclusionFee()` at
    /// `FeeBumpTransactionFrame.cpp:514-522` returns
    /// `getFullFee() - declaredSorobanResourceFee()`, where
    /// `declaredSorobanResourceFee` delegates to the inner Soroban tx
    /// (`FeeBumpTransactionFrame.cpp:507-512`).
    ///
    /// Regression test for AUDIT-214 (#2103).
    #[test]
    fn test_fee_bump_soroban_zero_inclusion_rejected() {
        let queue = TransactionQueue::with_defaults();
        // Inner Soroban tx has resource_fee=500. The outer envelope's
        // declaredSorobanResourceFee delegates to the inner, so the outer
        // inclusion_fee = outer_fee - inner_resource_fee.
        let inner_env = make_soroban_envelope_with_resource_fee(500, 500, 100);
        let inner_v1 = match inner_env {
            TransactionEnvelope::Tx(env) => env,
            _ => panic!("expected V1 envelope"),
        };
        // outer fee = 500 = inner resource fee → outer inclusion_fee = 0.
        let fb = make_fee_bump_envelope(inner_v1, 200, 500);
        assert_eq!(queue.try_add(fb), TxQueueResult::FeeTooLow);
    }

    /// Roundtrip regression for AUDIT-214 (#2103): an admitted Soroban
    /// transaction must produce a tx set whose every phase passes
    /// `check_fee_map`. This guards against admission and tx-set
    /// validation drifting apart on inclusion-fee semantics.
    #[test]
    fn test_admitted_soroban_tx_passes_check_fee_map() {
        use crate::tx_set_utils::{check_fee_map, TxSetValidationResult};

        let queue = TransactionQueue::with_defaults();
        let envelope = make_soroban_envelope_with_resource_fee(600, 500, 100);
        assert_eq!(queue.try_add(envelope), TxQueueResult::Added);

        let tx_set = queue.build_generalized_tx_set(Hash256::ZERO, 1000);
        let gen = tx_set
            .generalized_tx_set()
            .expect("generalized tx set")
            .clone();
        let GeneralizedTransactionSet::V1(v1) = gen;
        let lcl_base_fee = queue.validation_context.read().base_fee;
        for phase in v1.phases.iter() {
            assert_eq!(
                check_fee_map(phase, lcl_base_fee),
                TxSetValidationResult::Valid,
                "every phase must pass check_fee_map after the fix"
            );
        }
    }

    /// Negative roundtrip for AUDIT-214 (#2103): a zero-inclusion Soroban
    /// transaction is rejected at admission and never reaches the built
    /// tx set, so it cannot trigger the herder's own `check_fee_map`
    /// rejection.
    #[test]
    fn test_zero_inclusion_soroban_never_reaches_tx_set() {
        let queue = TransactionQueue::with_defaults();
        let envelope = make_soroban_envelope_with_resource_fee(500, 500, 100);
        assert_eq!(queue.try_add(envelope), TxQueueResult::FeeTooLow);

        let tx_set = queue.build_generalized_tx_set(Hash256::ZERO, 1000);
        let gen = tx_set
            .generalized_tx_set()
            .expect("generalized tx set")
            .clone();
        let GeneralizedTransactionSet::V1(v1) = gen;
        let total: usize = v1.phases.iter().map(audit_214_phase_tx_count).sum();
        assert_eq!(total, 0, "rejected tx must not appear in built tx set");
    }

    /// Helper for AUDIT-214 (#2103) tests: count txs in a phase.
    fn audit_214_phase_tx_count(phase: &TransactionPhase) -> usize {
        use stellar_xdr::TxSetComponent;
        match phase {
            TransactionPhase::V0(components) => components
                .iter()
                .map(|c| {
                    let TxSetComponent::TxsetCompTxsMaybeDiscountedFee(d) = c;
                    d.txs.len()
                })
                .sum(),
            TransactionPhase::V1(parallel) => parallel
                .execution_stages
                .iter()
                .flat_map(|stage| stage.iter())
                .map(|cluster| cluster.len())
                .sum(),
        }
    }

    // =========================================================================
    // Phase 3A: check_soroban_resources tests
    // =========================================================================

    fn make_soroban_frame_with_resources(
        instructions: u32,
        disk_read_bytes: u32,
        write_bytes: u32,
        read_only_entries: usize,
        read_write_entries: usize,
    ) -> TransactionEnvelope {
        use stellar_xdr::LedgerKey;
        let source = MuxedAccount::Ed25519(Uint256([50u8; 32]));
        let host_function = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address: ScAddress::default(),
            function_name: ScSymbol(StringM::<32>::try_from("test".to_string()).expect("symbol")),
            args: VecM::<ScVal>::default(),
        });
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function,
                auth: VecM::default(),
            }),
        };

        // Build footprint with specified entry counts
        let read_only: Vec<LedgerKey> = (0..read_only_entries)
            .map(|i| {
                LedgerKey::Account(stellar_xdr::LedgerKeyAccount {
                    account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([i as u8; 32]))),
                })
            })
            .collect();
        let read_write: Vec<LedgerKey> = (0..read_write_entries)
            .map(|i| {
                LedgerKey::Account(stellar_xdr::LedgerKeyAccount {
                    account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
                        [(i + 100) as u8; 32],
                    ))),
                })
            })
            .collect();

        let resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: read_only.try_into().unwrap(),
                read_write: read_write.try_into().unwrap(),
            },
            instructions,
            disk_read_bytes,
            write_bytes,
        };
        let tx = Transaction {
            source_account: source,
            fee: 10_000,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V1(SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources,
                resource_fee: 5000,
            }),
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        envelope
    }

    fn make_queue_with_soroban_limits(limits: SorobanTxLimits) -> TransactionQueue {
        let queue = TransactionQueue::with_defaults();
        queue.validation_context.write().soroban_limits = Some(limits);
        queue
    }

    fn permissive_soroban_limits() -> SorobanTxLimits {
        SorobanTxLimits {
            tx_max_instructions: 1_000_000,
            tx_max_read_bytes: 1_000_000,
            tx_max_write_bytes: 1_000_000,
            tx_max_read_ledger_entries: 100,
            tx_max_write_ledger_entries: 50,
            tx_max_size_bytes: 1_000_000,
        }
    }

    #[test]
    fn test_check_soroban_resources_passes_within_limits() {
        let queue = make_queue_with_soroban_limits(permissive_soroban_limits());
        let frame = make_soroban_frame_with_resources(100, 200, 300, 2, 1);
        assert!(queue.check_soroban_resources(&frame).is_ok());
    }

    #[test]
    fn test_check_soroban_resources_no_limits_skips_check() {
        let queue = TransactionQueue::with_defaults();
        // No soroban_limits configured — should pass
        let frame = make_soroban_frame_with_resources(u32::MAX, u32::MAX, u32::MAX, 100, 100);
        assert!(queue.check_soroban_resources(&frame).is_ok());
    }

    #[test]
    fn test_check_soroban_resources_rejects_excess_instructions() {
        let mut limits = permissive_soroban_limits();
        limits.tx_max_instructions = 50;
        let queue = make_queue_with_soroban_limits(limits);

        let frame = make_soroban_frame_with_resources(100, 0, 0, 0, 0);
        let err = queue.check_soroban_resources(&frame).unwrap_err();
        assert!(err.contains("instructions"), "Error: {}", err);
    }

    #[test]
    fn test_check_soroban_resources_rejects_excess_read_bytes() {
        let mut limits = permissive_soroban_limits();
        limits.tx_max_read_bytes = 100;
        let queue = make_queue_with_soroban_limits(limits);

        let frame = make_soroban_frame_with_resources(0, 200, 0, 0, 0);
        let err = queue.check_soroban_resources(&frame).unwrap_err();
        assert!(err.contains("read bytes"), "Error: {}", err);
    }

    #[test]
    fn test_check_soroban_resources_rejects_excess_write_bytes() {
        let mut limits = permissive_soroban_limits();
        limits.tx_max_write_bytes = 100;
        let queue = make_queue_with_soroban_limits(limits);

        let frame = make_soroban_frame_with_resources(0, 0, 200, 0, 0);
        let err = queue.check_soroban_resources(&frame).unwrap_err();
        assert!(err.contains("write bytes"), "Error: {}", err);
    }

    #[test]
    fn test_check_soroban_resources_rejects_excess_write_entries() {
        let mut limits = permissive_soroban_limits();
        limits.tx_max_write_ledger_entries = 2;
        let queue = make_queue_with_soroban_limits(limits);

        let frame = make_soroban_frame_with_resources(0, 0, 0, 0, 5);
        let err = queue.check_soroban_resources(&frame).unwrap_err();
        assert!(err.contains("write entries"), "Error: {}", err);
    }

    #[test]
    fn test_check_soroban_resources_rejects_excess_total_read_entries() {
        let mut limits = permissive_soroban_limits();
        limits.tx_max_read_ledger_entries = 5;
        let queue = make_queue_with_soroban_limits(limits);

        // 4 read-only + 3 read-write = 7 total > 5 limit
        let frame = make_soroban_frame_with_resources(0, 0, 0, 4, 3);
        let err = queue.check_soroban_resources(&frame).unwrap_err();
        assert!(err.contains("read entries"), "Error: {}", err);
    }

    /// Regression test for AUDIT-072: seen-set hashes never clear on eviction.
    ///
    /// Before the fix, evicting a tx left its hash in the `seen` set, so
    /// re-adding the same tx after eviction returned `Duplicate` instead of
    /// `Added`.
    #[test]
    fn test_audit_072_seen_cleared_on_eviction() {
        // Queue with max_size=2 to force fee-rate eviction
        let queue = TransactionQueue::with_max_size(2);

        // All txs need different sources to avoid per-account limit
        let mut tx1 = make_test_envelope(100, 1); // low fee — eviction candidate
        set_source(&mut tx1, 1);
        let hash1 = Hash256::hash_xdr(&tx1);
        let mut tx2 = make_test_envelope(200, 1);
        set_source(&mut tx2, 2);
        let mut tx3 = make_test_envelope(300, 1);
        set_source(&mut tx3, 3);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        assert!(queue.seen.read().contains(&hash1));
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        // tx3 has higher fee than tx1, so tx1 gets fee-rate evicted
        assert_eq!(queue.try_add(tx3), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        // Before fix: hash1 remained in seen forever.
        // After fix: hash1 is removed from seen on eviction.
        assert!(
            !queue.seen.read().contains(&hash1),
            "evicted tx hash should be removed from seen set"
        );
    }

    /// Regression test for AUDIT-072: ban() clears seen set.
    #[test]
    fn test_audit_072_seen_cleared_on_ban() {
        let queue = TransactionQueue::with_ban_depth(TxQueueConfig::default(), 3);

        let tx1 = make_test_envelope(200, 1);
        let hash1 = Hash256::hash_xdr(&tx1);

        assert_eq!(queue.try_add(tx1.clone()), TxQueueResult::Added);

        // Ban tx1
        queue.ban(&[hash1]);
        assert_eq!(queue.len(), 0);
        assert!(queue.is_banned(&hash1));

        // Shift 3 times to unban (ban_depth=3)
        queue.shift();
        queue.shift();
        queue.shift();
        assert!(!queue.is_banned(&hash1));

        // Before fix: re-add would return Duplicate even after unban
        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
    }

    /// Regression test for AUDIT-072: evict_expired() clears seen set.
    #[test]
    fn test_audit_072_seen_cleared_on_evict_expired() {
        let config = TxQueueConfig {
            max_age_secs: 1,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx = make_test_envelope(200, 1);
        set_source(&mut tx, 10);
        let hash = Hash256::hash_xdr(&tx);

        assert_eq!(queue.try_add(tx), TxQueueResult::Added);
        assert!(queue.seen.read().contains(&hash));

        // Artificially expire the transaction
        {
            let mut store = queue.store.write();
            for tx in store.values_mut() {
                tx.received_at = tx
                    .received_at
                    .checked_sub(std::time::Duration::from_secs(10))
                    .unwrap_or_else(|| {
                        std::time::Instant::now() - std::time::Duration::from_secs(10)
                    });
            }
        }
        queue.evict_expired();
        assert!(queue.is_empty());

        assert!(
            !queue.seen.read().contains(&hash),
            "expired tx hash should be removed from seen set"
        );
    }

    /// Regression test for AUDIT-072: remove_applied() clears seen set.
    #[test]
    fn test_audit_072_seen_cleared_on_remove_applied() {
        let queue = TransactionQueue::with_defaults();

        let mut tx = make_test_envelope(200, 1);
        set_source(&mut tx, 20);
        let hash = Hash256::hash_xdr(&tx);

        assert_eq!(queue.try_add(tx.clone()), TxQueueResult::Added);
        assert!(queue.seen.read().contains(&hash));

        queue.remove_applied(&[(tx.clone(), 1)]);
        assert_eq!(queue.len(), 0);

        assert!(
            !queue.seen.read().contains(&hash),
            "applied tx hash should be removed from seen set"
        );
    }

    /// Regression test for AUDIT-072: shift() auto-ban clears seen set.
    #[test]
    fn test_audit_072_seen_cleared_on_shift_autoban() {
        // pending_depth=1 so the first shift auto-bans
        let queue = TransactionQueue::with_depths(TxQueueConfig::default(), 10, 1);

        let mut tx = make_test_envelope(200, 1);
        set_source(&mut tx, 30);
        let hash = Hash256::hash_xdr(&tx);

        assert_eq!(queue.try_add(tx), TxQueueResult::Added);
        assert!(queue.seen.read().contains(&hash));

        let result = queue.shift();
        assert_eq!(result.evicted_due_to_age, 1);
        assert_eq!(queue.len(), 0);

        assert!(
            !queue.seen.read().contains(&hash),
            "shift-evicted tx hash should be removed from seen set"
        );
    }

    /// Regression test for AUDIT-006: lane eviction cleans up account state.
    /// Before fix, lane-evicted txs left ghost account_states entries.
    #[test]
    fn test_audit_006_lane_eviction_cleans_account_state() {
        // Queue with ops limit of 1 to force lane eviction when adding a 1-op tx
        // after a 1-op tx is already present.
        let config = TxQueueConfig {
            max_queue_ops: Some(1),
            max_size: 10,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // tx1: low-fee from source 1
        let mut tx1 = make_test_envelope(100, 1);
        set_source(&mut tx1, 1);

        // tx2: high-fee from source 2 — will lane-evict tx1
        let mut tx2 = make_test_envelope(500, 1);
        set_source(&mut tx2, 2);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        assert_eq!(queue.account_states.read().len(), 1);

        // tx2 evicts tx1 via lane eviction (ops limit exceeded)
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);
        assert_eq!(queue.len(), 1);

        // After fix: only source 2 remains, no ghost state for source 1.
        assert_eq!(
            queue.account_states.read().len(),
            1,
            "lane-evicted tx's account state should be cleaned up"
        );
    }

    /// Regression test for AUDIT-006: expired-tx eviction in try_add cleans up
    /// account state. Before fix, expired txs removed during try_add's size
    /// check left ghost account_states entries.
    #[test]
    fn test_audit_006_expired_eviction_cleans_account_state() {
        // Queue with max_size=1 and max_age_secs=0 so existing txs are expired
        let config = TxQueueConfig {
            max_size: 1,
            max_age_secs: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // tx1: from source 1 — will become expired
        let mut tx1 = make_test_envelope(100, 1);
        set_source(&mut tx1, 1);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        assert_eq!(queue.account_states.read().len(), 1);

        // Make tx1 expired by backdating received_at
        {
            let mut store = queue.store.write();
            for tx in store.values_mut() {
                tx.received_at = tx
                    .received_at
                    .checked_sub(std::time::Duration::from_secs(10))
                    .unwrap_or_else(|| {
                        std::time::Instant::now() - std::time::Duration::from_secs(10)
                    });
            }
        }

        // tx2: from source 2 — try_add will first evict expired tx1, then add tx2
        let mut tx2 = make_test_envelope(200, 1);
        set_source(&mut tx2, 2);

        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);
        assert_eq!(queue.len(), 1);

        // After fix: only source 2 remains, expired tx1's state is cleaned up.
        assert_eq!(
            queue.account_states.read().len(),
            1,
            "expired tx's account state should be cleaned up during try_add"
        );
    }

    /// Regression test for AUDIT-006: fee-rate eviction cleans up account state.
    /// Before fix, evicted txs left ghost account_states entries, blocking
    /// future submissions from the same account with TryAgainLater.
    #[test]
    fn test_audit_006_eviction_cleans_account_state() {
        // Queue with max_size=1 to force eviction on second add
        let queue = TransactionQueue::with_max_size(1);

        // tx1: low-fee from source 1
        let mut tx1 = make_test_envelope(100, 1);
        set_source(&mut tx1, 1);

        // tx2: high-fee from source 2 — will evict tx1
        let mut tx2 = make_test_envelope(500, 1);
        set_source(&mut tx2, 2);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        // 1 account state entry (source 1 = both seq-source and fee-source)
        assert_eq!(queue.account_states.read().len(), 1);

        // tx2 evicts tx1 via fee-rate eviction
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);
        assert_eq!(queue.len(), 1);

        // Before fix: account_states had 2 entries (ghost state for source 1).
        // After fix: only source 2 remains.
        assert_eq!(
            queue.account_states.read().len(),
            1,
            "evicted tx's account state should be cleaned up"
        );
    }

    #[test]
    fn test_evicted_transactions_are_banned() {
        // Regression test for AUDIT-120: evicted transactions must be banned
        // so they cannot be immediately re-submitted after shift().
        // Parity: stellar-core TransactionQueue.cpp:733-739.
        let config = TxQueueConfig {
            max_queue_ops: Some(1),
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx_low = make_test_envelope(100, 1);
        let mut tx_high = make_test_envelope(400, 1);
        set_source(&mut tx_low, 91);
        set_source(&mut tx_high, 92);

        let low_hash = full_hash(&tx_low);

        // Add low-fee tx, then high-fee tx evicts it.
        assert_eq!(queue.try_add(tx_low.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_high.clone()), TxQueueResult::Added);
        assert!(!queue.contains(&low_hash));

        // Evicted tx must be banned.
        assert!(
            queue.is_banned(&low_hash),
            "Evicted tx should be banned to prevent immediate re-submission"
        );

        // Even after shift() resets thresholds, re-submission should be rejected.
        queue.shift();
        assert!(
            queue.is_banned(&low_hash),
            "Evicted tx should remain banned after one shift()"
        );
    }

    /// Test that the fee index stays consistent after a sequence of operations.
    #[test]
    fn test_fee_index_consistency_after_mixed_operations() {
        let config = TxQueueConfig {
            max_size: 10,
            max_age_secs: 60,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Add several transactions
        for i in 1..=5u64 {
            let mut tx = make_test_envelope(100 * i as u32, 1);
            set_source(&mut tx, i as u8);
            assert_eq!(queue.try_add(tx), TxQueueResult::Added);
        }
        queue.store.read().assert_consistent();

        // Remove via ban
        let hash = {
            let store = queue.store.read();
            let h = store.iter().next().map(|(h, _)| *h).unwrap();
            h
        };
        queue.ban(&[hash]);
        queue.store.read().assert_consistent();
        assert_eq!(queue.store.read().len(), 4);

        // Shift (age-out)
        queue.shift();
        queue.store.read().assert_consistent();

        // Clear
        queue.clear();
        queue.store.read().assert_consistent();
        assert_eq!(queue.store.read().len(), 0);
    }

    /// Test that ensure_queue_capacity evicts the lowest-fee transaction.
    #[test]
    fn test_ensure_capacity_evicts_lowest_fee() {
        let config = TxQueueConfig {
            max_size: 3,
            max_age_secs: 600,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Fill queue with fees 100, 200, 300
        for i in 1..=3u64 {
            let mut tx = make_test_envelope(100 * i as u32, 1);
            set_source(&mut tx, i as u8);
            assert_eq!(queue.try_add(tx), TxQueueResult::Added);
        }
        assert_eq!(queue.len(), 3);

        // Add tx with fee 400 — should evict fee=100
        let mut tx4 = make_test_envelope(400, 1);
        set_source(&mut tx4, 4);
        assert_eq!(queue.try_add(tx4), TxQueueResult::Added);
        assert_eq!(queue.len(), 3);

        // Verify fee=100 was evicted (lowest fee)
        let store = queue.store.read();
        let fees: Vec<i64> = store.values().map(|tx| tx.inclusion_fee_i64()).collect();
        assert!(
            !fees.contains(&100),
            "lowest-fee tx should have been evicted"
        );
        assert!(fees.contains(&200));
        assert!(fees.contains(&300));
        assert!(fees.contains(&400));
        store.assert_consistent();
    }

    /// Test that ensure_queue_capacity falls back to expired eviction when
    /// the incoming tx has a worse fee than all queued txs.
    #[test]
    fn test_ensure_capacity_expired_fallback() {
        let config = TxQueueConfig {
            max_size: 1,
            max_age_secs: 0, // immediate expiry
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Add a high-fee transaction
        let mut tx1 = make_test_envelope(1000, 1);
        set_source(&mut tx1, 1);
        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);

        // Backdate to make it expired
        {
            let mut store = queue.store.write();
            for tx in store.values_mut() {
                tx.received_at = tx
                    .received_at
                    .checked_sub(std::time::Duration::from_secs(10))
                    .unwrap_or_else(|| {
                        std::time::Instant::now() - std::time::Duration::from_secs(10)
                    });
            }
        }

        // Add a LOW-fee tx — would normally be rejected by fee comparison,
        // but the expired fallback should evict the high-fee expired tx.
        let mut tx2 = make_test_envelope(50, 1);
        set_source(&mut tx2, 2);
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);
        assert_eq!(queue.len(), 1);

        // Verify the new tx is the one in the queue
        let store = queue.store.read();
        let remaining: Vec<i64> = store.values().map(|tx| tx.inclusion_fee_i64()).collect();
        assert_eq!(remaining, vec![50]);
        store.assert_consistent();
    }

    /// Test that ensure_queue_capacity prefers evicting an expired high-fee tx
    /// over a live low-fee tx when both are present in a full queue.
    #[test]
    fn test_ensure_capacity_prefers_expired_over_live() {
        let config = TxQueueConfig {
            max_size: 2,
            max_age_secs: 5,
            min_fee_per_op: 0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Add a high-fee tx (will be backdated to expired)
        let mut tx_high = make_test_envelope(1000, 1);
        set_source(&mut tx_high, 1);
        assert_eq!(queue.try_add(tx_high), TxQueueResult::Added);

        // Add a low-fee tx (will remain live)
        let mut tx_low = make_test_envelope(10, 1);
        set_source(&mut tx_low, 2);
        assert_eq!(queue.try_add(tx_low), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        // Backdate only the high-fee tx to make it expired
        {
            let mut store = queue.store.write();
            // Find the high-fee tx and backdate it
            for tx in store.values_mut() {
                if tx.inclusion_fee_i64() == 1000 {
                    tx.received_at = std::time::Instant::now() - std::time::Duration::from_secs(10);
                }
            }
        }

        // Add a mid-fee tx — should evict the expired high-fee tx, NOT the live low-fee tx
        let mut tx_mid = make_test_envelope(500, 1);
        set_source(&mut tx_mid, 3);
        assert_eq!(queue.try_add(tx_mid), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        // Verify: expired high-fee (1000) was evicted, live low-fee (10) and new mid (500) remain
        let store = queue.store.read();
        let mut fees: Vec<i64> = store.values().map(|tx| tx.inclusion_fee_i64()).collect();
        fees.sort();
        assert_eq!(
            fees,
            vec![10, 500],
            "expired tx should be evicted over live tx"
        );
        store.assert_consistent();
    }

    /// Regression test for issue #2296: when surge pricing rejects ALL Soroban
    /// txs (soroban_limited=true, empty survivors), the emitted phase must have
    /// base_fee=None — matching stellar-core's behavior where an empty
    /// inclusionFeeMap leaves baseFee as std::nullopt.
    #[test]
    fn test_empty_surge_priced_soroban_phase_has_no_base_fee() {
        // Set Soroban instruction limit to 1 — too small for any tx to fit.
        let mut limit = Resource::new(vec![i64::MAX; NUM_SOROBAN_TX_RESOURCES]);
        limit.set_val(ResourceType::Instructions, 1);
        let config = TxQueueConfig {
            max_soroban_resources: Some(limit),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Add a Soroban tx that requires 80 instructions — will be rejected.
        let mut tx = make_soroban_envelope_with_resources(8000, 80);
        set_source(&mut tx, 99);
        queue.try_add(tx);

        let _set = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let gen = _set.generalized_tx_set().unwrap().clone();
        let stellar_xdr::GeneralizedTransactionSet::V1(v1) = gen;
        let parallel = match &v1.phases[1] {
            stellar_xdr::TransactionPhase::V1(parallel) => parallel,
            _ => panic!("expected Soroban V1 phase"),
        };

        // The phase should have no execution stages (all txs rejected).
        assert!(
            parallel.execution_stages.is_empty(),
            "expected empty execution stages when all Soroban txs rejected"
        );
        // Invariant: empty execution_stages → base_fee must be None.
        assert_eq!(
            parallel.base_fee, None,
            "empty surge-priced Soroban phase must have base_fee=None (stellar-core parity)"
        );
    }

    /// Regression test for #2690: cross-type RBF (soroban pending, classic
    /// fee-bump submitted) must return TryAgainLater, not panic.
    #[test]
    fn test_cross_type_rbf_soroban_to_classic_rejected() {
        let queue = TransactionQueue::with_defaults();

        // Queue a soroban tx from account seed 42, seq 1.
        let mut soroban = make_soroban_envelope(200);
        set_source(&mut soroban, 42);
        if let TransactionEnvelope::Tx(env) = &mut soroban {
            env.tx.seq_num = SequenceNumber(1);
        }
        assert_eq!(queue.try_add(soroban.clone()), TxQueueResult::Added);
        let soroban_hash = full_hash(&soroban);

        // Build a classic fee-bump wrapping a classic inner tx from the same
        // account (seed 42), same seq 1, with 10x fee.
        let mut inner = match make_test_envelope(100, 1) {
            TransactionEnvelope::Tx(env) => env,
            _ => panic!("expected Tx"),
        };
        inner.tx.source_account = MuxedAccount::Ed25519(Uint256([42; 32]));
        inner.tx.seq_num = SequenceNumber(1);
        let classic_bump = make_fee_bump_envelope(inner, 42, 10000);

        // Cross-type submission must be rejected (parity: HerderImpl.cpp:627-642).
        let result = queue.try_add(classic_bump);
        assert_eq!(result, TxQueueResult::TryAgainLater);

        // Original soroban tx must remain queued.
        assert!(queue.contains(&soroban_hash));
        assert_eq!(queue.len(), 1);
    }

    /// Symmetric case of #2690: classic pending, soroban submitted.
    #[test]
    fn test_cross_type_rbf_classic_to_soroban_rejected() {
        let queue = TransactionQueue::with_defaults();

        // Queue a classic tx from account seed 42, seq 1.
        let mut classic = make_test_envelope(200, 1);
        set_source(&mut classic, 42);
        if let TransactionEnvelope::Tx(env) = &mut classic {
            env.tx.seq_num = SequenceNumber(1);
        }
        assert_eq!(queue.try_add(classic.clone()), TxQueueResult::Added);
        let classic_hash = full_hash(&classic);

        // Submit a soroban tx from the same account, same seq.
        // Not a fee-bump, so it would normally get TryAgainLater from
        // the !is_fee_bump check. But the cross-type check fires first.
        let mut soroban = make_soroban_envelope(2000);
        set_source(&mut soroban, 42);
        if let TransactionEnvelope::Tx(env) = &mut soroban {
            env.tx.seq_num = SequenceNumber(1);
        }

        let result = queue.try_add(soroban);
        assert_eq!(result, TxQueueResult::TryAgainLater);

        // Original classic tx must remain queued.
        assert!(queue.contains(&classic_hash));
        assert_eq!(queue.len(), 1);
    }

    /// Symmetric case of #2690: classic pending, soroban fee-bump submitted.
    /// This is the true symmetric regression test — uses a fee-bump
    /// (unlike test_cross_type_rbf_classic_to_soroban_rejected which
    /// uses a plain tx that would hit !is_fee_bump → TryAgainLater anyway).
    #[test]
    fn test_cross_type_soroban_fee_bump_over_classic_rejected() {
        let queue = TransactionQueue::with_defaults();

        // Queue a classic tx from account seed 42, seq 1.
        let mut classic = make_test_envelope(200, 1);
        set_source(&mut classic, 42);
        if let TransactionEnvelope::Tx(env) = &mut classic {
            env.tx.seq_num = SequenceNumber(1);
        }
        assert_eq!(queue.try_add(classic.clone()), TxQueueResult::Added);
        let classic_hash = full_hash(&classic);

        // Build a soroban fee-bump wrapping a soroban inner from the same
        // account (seed 42), same seq 1, with 10x fee.
        let mut soroban_inner = match make_soroban_envelope(100) {
            TransactionEnvelope::Tx(env) => env,
            _ => panic!("expected Tx"),
        };
        soroban_inner.tx.source_account = MuxedAccount::Ed25519(Uint256([42; 32]));
        soroban_inner.tx.seq_num = SequenceNumber(1);
        let soroban_bump = make_fee_bump_envelope(soroban_inner, 42, 10000);

        let result = queue.try_add(soroban_bump);
        assert_eq!(result, TxQueueResult::TryAgainLater);

        // Original classic tx must remain queued.
        assert!(queue.contains(&classic_hash));
        assert_eq!(queue.len(), 1);
    }

    /// Cross-type rejection must win over FeeTooLow: even with insufficient
    /// fee, the result is TryAgainLater (not FeeTooLow) because the type
    /// check precedes fee evaluation.
    #[test]
    fn test_cross_type_low_fee_returns_try_again_not_fee_too_low() {
        let queue = TransactionQueue::with_defaults();

        // Queue a soroban tx from account seed 42, seq 1, fee 200.
        let mut soroban = make_soroban_envelope(200);
        set_source(&mut soroban, 42);
        if let TransactionEnvelope::Tx(env) = &mut soroban {
            env.tx.seq_num = SequenceNumber(1);
        }
        assert_eq!(queue.try_add(soroban), TxQueueResult::Added);

        // Build a classic fee-bump with fee that passes the LCL base fee
        // check but would fail the RBF 10x requirement.
        let mut inner = match make_test_envelope(100, 1) {
            TransactionEnvelope::Tx(env) => env,
            _ => panic!("expected Tx"),
        };
        inner.tx.source_account = MuxedAccount::Ed25519(Uint256([42; 32]));
        inner.tx.seq_num = SequenceNumber(1);
        // Outer fee = 300 passes global base fee check (100 per op * 2 ops = 200),
        // but fee_rate = 300/2 = 150, far below 10x of soroban's 200 fee_rate.
        // Without cross-type guard this would reach can_replace_by_fee → FeeTooLow.
        let classic_bump = make_fee_bump_envelope(inner, 42, 300);

        let result = queue.try_add(classic_bump);
        assert_eq!(
            result,
            TxQueueResult::TryAgainLater,
            "cross-type check must precede fee check"
        );
    }

    /// Same-type classic RBF still works after the cross-type guard.
    #[test]
    fn test_same_type_classic_rbf_still_works() {
        let queue = TransactionQueue::with_defaults();

        // Queue a classic tx from account seed 42, seq 1, fee 200.
        let mut classic = make_test_envelope(200, 1);
        set_source(&mut classic, 42);
        if let TransactionEnvelope::Tx(env) = &mut classic {
            env.tx.seq_num = SequenceNumber(1);
        }
        assert_eq!(queue.try_add(classic.clone()), TxQueueResult::Added);
        let old_hash = full_hash(&classic);

        // Fee-bump with same type (classic), same account, same seq, 10x+ fee.
        let mut inner = match make_test_envelope(100, 1) {
            TransactionEnvelope::Tx(env) => env,
            _ => panic!("expected Tx"),
        };
        inner.tx.source_account = MuxedAccount::Ed25519(Uint256([42; 32]));
        inner.tx.seq_num = SequenceNumber(1);
        let bump = make_fee_bump_envelope(inner, 42, 10000);

        let result = queue.try_add(bump.clone());
        assert_eq!(result, TxQueueResult::Added);

        // Old tx should be replaced.
        assert!(!queue.contains(&old_hash));
        assert_eq!(queue.len(), 1);
    }

    /// Parity regression: queue a valid Soroban tx, then submit a classic tx
    /// from the same source with unsatisfied extra_signers. On current main
    /// this returns Invalid(Some(TxBadAuth)) because validate_transaction runs
    /// before the cross-type guard. After the fix, the early cross-type check
    /// returns TryAgainLater before validation.
    #[test]
    fn test_cross_type_invalid_classic_returns_try_again_later() {
        let queue = TransactionQueue::with_defaults();

        // Queue a valid Soroban tx from account seed 42, seq 1.
        let mut soroban = make_soroban_envelope(200);
        set_source(&mut soroban, 42);
        if let TransactionEnvelope::Tx(env) = &mut soroban {
            env.tx.seq_num = SequenceNumber(1);
        }
        assert_eq!(queue.try_add(soroban.clone()), TxQueueResult::Added);
        let soroban_hash = full_hash(&soroban);

        // Build a classic tx from same source (seed 42) with an unsatisfied
        // extra_signers precondition — this makes validate_transaction return
        // TxBadAuth on current main (the bug).
        let extra_signer = SignerKey::Ed25519(Uint256([99u8; 32])); // nobody signs with this key
        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: None,
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![extra_signer].try_into().unwrap(),
        });

        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([42u8; 32])),
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: preconditions,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([255u8; 32]))),
                    starting_balance: 1000000000,
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };

        let classic = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        // Parity: stellar-core returns TryAgainLater here because the
        // cross-queue source-account check fires before validation.
        let result = queue.try_add(classic);
        assert_eq!(
            result,
            TxQueueResult::TryAgainLater,
            "cross-type guard must precede validate_transaction"
        );

        // Original Soroban tx must remain queued.
        assert!(queue.contains(&soroban_hash));
        assert_eq!(queue.len(), 1);
    }

    /// Symmetric case: queue a valid classic tx, then submit a Soroban tx
    /// from the same source with unsatisfied extra_signers.
    #[test]
    fn test_cross_type_invalid_soroban_returns_try_again_later() {
        let queue = TransactionQueue::with_defaults();

        // Queue a valid classic tx from account seed 42, seq 1.
        let mut classic = make_test_envelope(200, 1);
        set_source(&mut classic, 42);
        if let TransactionEnvelope::Tx(env) = &mut classic {
            env.tx.seq_num = SequenceNumber(1);
        }
        assert_eq!(queue.try_add(classic.clone()), TxQueueResult::Added);
        let classic_hash = full_hash(&classic);

        // Build a Soroban tx from same source (seed 42) with an unsatisfied
        // extra_signers precondition.
        let extra_signer = SignerKey::Ed25519(Uint256([99u8; 32]));
        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: None,
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![extra_signer].try_into().unwrap(),
        });

        let function_name = ScSymbol(StringM::<32>::try_from("test".to_string()).expect("symbol"));
        let host_function = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address: ScAddress::default(),
            function_name,
            args: VecM::<ScVal>::default(),
        });
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function,
                auth: VecM::default(),
            }),
        };

        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256([42u8; 32])),
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: preconditions,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let soroban = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: XdrSignature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        // Parity: stellar-core returns TryAgainLater here.
        let result = queue.try_add(soroban);
        assert_eq!(
            result,
            TxQueueResult::TryAgainLater,
            "cross-type guard must precede validate_transaction"
        );

        // Original classic tx must remain queued.
        assert!(queue.contains(&classic_hash));
        assert_eq!(queue.len(), 1);
    }

    /// Fee-source-only cross-type guard: an account that only *fee-pays*
    /// an opposite-type tx must also trigger TryAgainLater.
    ///
    /// Parity: stellar-core's `sourceAccountPending` checks the opposite
    /// queue's `mAccountStates`, which includes fee-source-only entries.
    /// Before this fix, henyey's check only fired when `state.transaction`
    /// was set, missing the fee-source-only case.
    #[test]
    fn test_cross_type_fee_source_only_classic_returns_try_again_later() {
        let queue = TransactionQueue::with_defaults();

        // Queue a Soroban tx with seq-source = account 10, fee-source = account 42.
        // This creates an account_states entry for account 42 with
        // transaction = None but total_fees > 0 and soroban_fee_tx_count = 1.
        let mut soroban = make_soroban_envelope(200);
        set_source(&mut soroban, 10);
        if let TransactionEnvelope::Tx(env) = &mut soroban {
            env.tx.seq_num = SequenceNumber(1);
        }
        let fee_bumped = make_fee_bump_envelope(
            match soroban {
                TransactionEnvelope::Tx(ref env) => env.clone(),
                _ => panic!("expected Tx"),
            },
            42, // fee-source seed
            500,
        );
        assert_eq!(queue.try_add(fee_bumped.clone()), TxQueueResult::Added);
        let original_hash = full_hash(&fee_bumped);

        // Now submit a classic tx as seq-source = account 42 (the fee-payer above).
        // Parity: stellar-core sees account 42 in the Soroban queue (fee-source entry)
        // and returns TryAgainLater.
        let mut classic = make_test_envelope(200, 1);
        set_source(&mut classic, 42);
        if let TransactionEnvelope::Tx(env) = &mut classic {
            env.tx.seq_num = SequenceNumber(1);
        }

        let result = queue.try_add(classic);
        assert_eq!(
            result,
            TxQueueResult::TryAgainLater,
            "fee-source-only cross-type guard must reject opposite-type submission"
        );

        // Original fee-bumped Soroban tx must remain queued.
        assert!(queue.contains(&original_hash));
        assert_eq!(queue.len(), 1);
    }

    /// Symmetric case: fee-source-only with classic tx queued, then Soroban submission.
    #[test]
    fn test_cross_type_fee_source_only_soroban_returns_try_again_later() {
        let queue = TransactionQueue::with_defaults();

        // Queue a classic tx with seq-source = account 10, fee-source = account 42.
        let mut classic = make_test_envelope(200, 1);
        set_source(&mut classic, 10);
        if let TransactionEnvelope::Tx(env) = &mut classic {
            env.tx.seq_num = SequenceNumber(1);
        }
        let fee_bumped = make_fee_bump_envelope(
            match classic {
                TransactionEnvelope::Tx(ref env) => env.clone(),
                _ => panic!("expected Tx"),
            },
            42, // fee-source seed
            500,
        );
        assert_eq!(queue.try_add(fee_bumped.clone()), TxQueueResult::Added);
        let original_hash = full_hash(&fee_bumped);

        // Now submit a Soroban tx as seq-source = account 42.
        let mut soroban = make_soroban_envelope(200);
        set_source(&mut soroban, 42);
        if let TransactionEnvelope::Tx(env) = &mut soroban {
            env.tx.seq_num = SequenceNumber(1);
        }

        let result = queue.try_add(soroban);
        assert_eq!(
            result,
            TxQueueResult::TryAgainLater,
            "fee-source-only cross-type guard must reject opposite-type submission"
        );

        // Original fee-bumped classic tx must remain queued.
        assert!(queue.contains(&original_hash));
        assert_eq!(queue.len(), 1);
    }
}

#[cfg(test)]
mod pending_depth_tests {
    use super::*;
    use stellar_xdr::{
        CreateAccountOp, DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody,
        Preconditions, SequenceNumber, SignatureHint, Transaction, TransactionEnvelope,
        TransactionExt, TransactionV1Envelope, Uint256,
    };

    fn make_test_envelope(fee: u32, ops: usize) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let operations: Vec<Operation> = (0..ops)
            .map(|_| Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    // Use destination [255; 32] so it differs from any test source
                    destination: stellar_xdr::AccountId(
                        stellar_xdr::PublicKey::PublicKeyTypeEd25519(Uint256([255u8; 32])),
                    ),
                    starting_balance: 1_000_000_000,
                }),
            })
            .collect();
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: stellar_xdr::Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn set_source(envelope: &mut TransactionEnvelope, seed: u8) {
        match envelope {
            TransactionEnvelope::Tx(env) => {
                env.tx.source_account = MuxedAccount::Ed25519(Uint256([seed; 32]));
            }
            _ => {}
        }
    }

    // =========================================================================
    // TRANSACTION_QUEUE_TIMEOUT_LEDGERS auto-ban tests
    // =========================================================================

    #[test]
    fn test_default_pending_depth_is_4() {
        assert_eq!(TRANSACTION_QUEUE_TIMEOUT_LEDGERS, 4);
    }

    /// Parity pin: both depth constants must match stellar-core
    /// `HerderImpl.cpp:65-66` exactly. A change here is a deliberate
    /// protocol-parity decision, not an incidental edit.
    #[test]
    fn test_transaction_queue_depth_constants_parity() {
        assert_eq!(TRANSACTION_QUEUE_BAN_LEDGERS, 10);
        assert_eq!(TRANSACTION_QUEUE_TIMEOUT_LEDGERS, 4);
    }

    #[test]
    fn test_pending_tx_not_auto_banned_before_depth() {
        // With pending_depth=4, a pending TX should survive 3 shifts
        let queue = TransactionQueue::with_depths(TxQueueConfig::default(), 10, 4);

        let mut tx = make_test_envelope(200, 1);
        set_source(&mut tx, 50);
        let hash = Hash256::hash_xdr(&tx);

        assert_eq!(queue.try_add(tx), TxQueueResult::Added);
        assert_eq!(queue.len(), 1);

        // Shift 3 times — TX should still be in the queue (age < pending_depth)
        for i in 1..=3 {
            let result = queue.shift();
            assert_eq!(
                result.evicted_due_to_age, 0,
                "Shift {} should not evict (age {} < pending_depth 4)",
                i, i
            );
            assert_eq!(
                queue.len(),
                1,
                "TX should still be in queue after shift {}",
                i
            );
        }

        // TX should not be banned
        assert!(!queue.is_banned(&hash));
    }

    #[test]
    fn test_pending_tx_auto_banned_at_depth() {
        // With pending_depth=4, a pending TX should be auto-banned on the 4th shift
        let queue = TransactionQueue::with_depths(TxQueueConfig::default(), 10, 4);

        let mut tx = make_test_envelope(200, 1);
        set_source(&mut tx, 51);
        let hash = Hash256::hash_xdr(&tx);

        assert_eq!(queue.try_add(tx.clone()), TxQueueResult::Added);

        // Shift 4 times — TX should be evicted on the 4th shift
        for _ in 0..3 {
            queue.shift();
        }
        let result = queue.shift();
        assert_eq!(
            result.evicted_due_to_age, 1,
            "4th shift should auto-ban the TX"
        );
        assert_eq!(queue.len(), 0, "Queue should be empty after auto-ban");

        // TX should be banned
        assert!(queue.is_banned(&hash));

        // Trying to re-add should fail (either Banned or Duplicate depending on seen set)
        let add_result = queue.try_add(tx);
        assert!(
            add_result == TxQueueResult::Banned || add_result == TxQueueResult::Duplicate,
            "Auto-banned TX should not be re-addable, got: {:?}",
            add_result
        );
    }

    #[test]
    fn test_pending_depth_1_evicts_immediately() {
        // With pending_depth=1, TX should be evicted on the very first shift
        let queue = TransactionQueue::with_depths(TxQueueConfig::default(), 10, 1);

        let mut tx = make_test_envelope(200, 1);
        set_source(&mut tx, 52);
        let hash = Hash256::hash_xdr(&tx);

        assert_eq!(queue.try_add(tx), TxQueueResult::Added);

        let result = queue.shift();
        assert_eq!(result.evicted_due_to_age, 1);
        assert!(queue.is_banned(&hash));
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn test_multiple_pending_txs_age_independently() {
        // Two TXs added at different times should age independently
        let queue = TransactionQueue::with_depths(TxQueueConfig::default(), 10, 3);

        // Add TX A
        let mut tx_a = make_test_envelope(200, 1);
        set_source(&mut tx_a, 60);
        let hash_a = Hash256::hash_xdr(&tx_a);
        assert_eq!(queue.try_add(tx_a), TxQueueResult::Added);

        // Shift once (TX A age = 1)
        queue.shift();

        // Add TX B (TX A age = 1, TX B age = 0)
        let mut tx_b = make_test_envelope(200, 1);
        set_source(&mut tx_b, 61);
        let hash_b = Hash256::hash_xdr(&tx_b);
        assert_eq!(queue.try_add(tx_b), TxQueueResult::Added);

        // Shift twice more (TX A age = 3, TX B age = 2)
        queue.shift();
        let result = queue.shift();

        // TX A should be evicted (age=3 >= pending_depth=3), TX B should not
        assert_eq!(result.evicted_due_to_age, 1);
        assert!(queue.is_banned(&hash_a), "TX A should be auto-banned");
        assert!(!queue.is_banned(&hash_b), "TX B should not be banned yet");
        assert_eq!(queue.len(), 1, "Only TX B should remain");

        // One more shift should evict TX B
        let result = queue.shift();
        assert_eq!(result.evicted_due_to_age, 1);
        assert!(queue.is_banned(&hash_b), "TX B should now be auto-banned");
        assert_eq!(queue.len(), 0);
    }

    #[test]
    fn test_shift_result_unbanned_count() {
        // Verify that unbanned_count correctly reports bans being rotated out
        let queue = TransactionQueue::with_depths(TxQueueConfig::default(), 2, 100);

        let mut tx = make_test_envelope(200, 1);
        set_source(&mut tx, 70);
        let hash = Hash256::hash_xdr(&tx);

        // Ban the TX
        queue.ban(&[hash]);
        assert!(queue.is_banned(&hash));

        // With ban_depth=2, shift twice to unban
        let r1 = queue.shift();
        assert_eq!(r1.unbanned_count, 0);
        let r2 = queue.shift();
        assert_eq!(r2.unbanned_count, 1);
        assert!(!queue.is_banned(&hash));
    }
}

#[cfg(test)]
mod snapshot_providers_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A counting provider that tracks how many times it is called.
    /// Used to verify that override providers are used instead of queue defaults.
    struct CountingFeeProvider {
        call_count: AtomicU64,
    }

    impl CountingFeeProvider {
        fn new() -> Self {
            Self {
                call_count: AtomicU64::new(0),
            }
        }
        fn calls(&self) -> u64 {
            self.call_count.load(Ordering::Relaxed)
        }
    }

    impl FeeBalanceProvider for CountingFeeProvider {
        fn get_available_balance(
            &self,
            _account_id: &AccountId,
        ) -> henyey_ledger::Result<Option<i64>> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            // Return a large balance so no tx is trimmed for fee reasons.
            Ok(Some(i64::MAX))
        }
    }

    struct CountingAccountProvider {
        call_count: AtomicU64,
    }

    impl CountingAccountProvider {
        fn new() -> Self {
            Self {
                call_count: AtomicU64::new(0),
            }
        }
        fn calls(&self) -> u64 {
            self.call_count.load(Ordering::Relaxed)
        }
    }

    impl AccountProvider for CountingAccountProvider {
        fn load_account(
            &self,
            _account_id: &AccountId,
        ) -> henyey_ledger::Result<Option<AccountEntry>> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            Ok(None)
        }
    }

    /// A provider that panics if called — used to verify the queue's stored
    /// providers are NOT consulted when override providers are supplied.
    struct PanicFeeProvider;

    impl FeeBalanceProvider for PanicFeeProvider {
        fn get_available_balance(
            &self,
            _account_id: &AccountId,
        ) -> henyey_ledger::Result<Option<i64>> {
            panic!("Queue's stored FeeBalanceProvider should not be called when override is set");
        }
    }

    struct PanicAccountProvider;

    impl AccountProvider for PanicAccountProvider {
        fn load_account(
            &self,
            _account_id: &AccountId,
        ) -> henyey_ledger::Result<Option<AccountEntry>> {
            panic!("Queue's stored AccountProvider should not be called when override is set");
        }
    }

    fn make_test_envelope_with_source(fee: u32, source_seed: u8) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([source_seed; 32]));
        let operations: Vec<Operation> = vec![Operation {
            source_account: None,
            body: OperationBody::CreateAccount(stellar_xdr::CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([255u8; 32]))),
                starting_balance: 1_000_000_000,
            }),
        }];
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: stellar_xdr::Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    use stellar_xdr::{
        AccountId, DecoratedSignature, Memo, MuxedAccount, Operation, OperationBody, Preconditions,
        PublicKey, SequenceNumber, SignatureHint, Transaction, TransactionEnvelope, TransactionExt,
        TransactionV1Envelope, Uint256,
    };

    #[test]
    fn test_override_providers_used_instead_of_queue_defaults() {
        // Set up queue — add txs first, then install panic providers.
        // If the override providers work correctly, the panic providers
        // should never be called during build_generalized_tx_set_with_providers.
        let queue = TransactionQueue::with_defaults();

        // Add some txs to the queue (before setting panic providers).
        for i in 1..=5 {
            let tx = make_test_envelope_with_source(1000 * i, i as u8);
            queue.try_add(tx);
        }

        // NOW set queue's stored providers to ones that panic.
        queue.set_fee_balance_provider(Arc::new(PanicFeeProvider));
        queue.set_account_provider(Arc::new(PanicAccountProvider));

        // Create counting override providers.
        let fee_provider = CountingFeeProvider::new();
        let account_provider = CountingAccountProvider::new();

        // Build tx set with override providers — should NOT panic.
        let _tx_set = queue.build_generalized_tx_set_with_providers(
            crate::tx_queue::selection::BuildContext::Queue,
            Hash256::ZERO,
            100,
            None,
            0,
            Some(&fee_provider),
            Some(&account_provider),
        );

        // The override providers should have been called (at least once
        // per source account for the fee provider during trim_invalid).
        assert!(
            fee_provider.calls() > 0 || account_provider.calls() > 0,
            "Override providers should be consulted during trim_invalid"
        );
    }

    #[test]
    fn test_no_override_uses_queue_defaults() {
        // When no override providers are given, queue's stored providers are used.
        let queue = TransactionQueue::with_defaults();

        let counting_fee = Arc::new(CountingFeeProvider::new());
        let counting_account = Arc::new(CountingAccountProvider::new());
        queue.set_fee_balance_provider(counting_fee.clone());
        queue.set_account_provider(counting_account.clone());

        for i in 1..=3 {
            let tx = make_test_envelope_with_source(1000 * i, i as u8);
            queue.try_add(tx);
        }

        // Build without override — should use queue's stored providers.
        let _tx_set = queue.build_generalized_tx_set_with_providers(
            crate::tx_queue::selection::BuildContext::Queue,
            Hash256::ZERO,
            100,
            None,
            0,
            None,
            None,
        );

        assert!(
            counting_fee.calls() > 0 || counting_account.calls() > 0,
            "Queue's stored providers should be consulted when no override"
        );
    }
}

/// Parity tests for resource-limit-based filtering in the parallel TxSet builder pipeline.
///
/// Ports stellar-core `TxSetTests.cpp:2727-2863`: the "no conflicts" resource-limit scenarios
/// that exercise surge pricing → parallel builder → base fee computation when ledger-wide
/// resource limits cause transaction eviction.
#[cfg(test)]
mod resource_limit_parity_tests {
    use super::*;
    use henyey_common::{Resource, ResourceType};
    use stellar_xdr::{
        ContractDataDurability, GeneralizedTransactionSet, HostFunction, InvokeContractArgs,
        InvokeHostFunctionOp, LedgerFootprint, LedgerKey, LedgerKeyContractData, Memo,
        MuxedAccount, Operation, OperationBody, Preconditions, ScAddress, ScVal, SorobanResources,
        SorobanTransactionData, SorobanTransactionDataExt, Transaction, TransactionEnvelope,
        TransactionExt, TransactionV1Envelope, Uint256, VecM, WriteXdr,
    };

    const STAGE_COUNT: u32 = 4;
    const CLUSTER_COUNT: u32 = 8;
    const LEDGER_MAX_INSTRUCTIONS: i64 = 400_000_000;

    /// Protocol version for parallel Soroban phase (v23).
    const PROTOCOL_VERSION: u32 = 23;

    /// Generate a contract data ledger key from an i32 ID.
    /// Durability alternates: even=Persistent, odd=Temporary (matches stellar-core).
    fn contract_data_key(id: i32) -> LedgerKey {
        let durability = if id % 2 == 0 {
            ContractDataDurability::Persistent
        } else {
            ContractDataDurability::Temporary
        };
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(stellar_xdr::ContractId(stellar_xdr::Hash([0u8; 32]))),
            key: ScVal::I32(id),
            durability,
        })
    }

    /// Create a Soroban TX with specified resource fields and unique source account.
    fn make_resource_limit_tx(
        account_id: &mut u32,
        instructions: u32,
        ro_keys: &[i32],
        rw_keys: &[i32],
        inclusion_fee: i64,
        disk_read_bytes: u32,
        write_bytes: u32,
    ) -> TransactionEnvelope {
        let id = *account_id;
        *account_id += 1;

        let mut source_bytes = [0u8; 32];
        source_bytes[..4].copy_from_slice(&id.to_le_bytes());
        let source = MuxedAccount::Ed25519(Uint256(source_bytes));

        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: ro_keys
                        .iter()
                        .map(|&k| contract_data_key(k))
                        .collect::<Vec<_>>()
                        .try_into()
                        .unwrap_or_default(),
                    read_write: rw_keys
                        .iter()
                        .map(|&k| contract_data_key(k))
                        .collect::<Vec<_>>()
                        .try_into()
                        .unwrap_or_default(),
                },
                instructions,
                disk_read_bytes,
                write_bytes,
            },
            resource_fee: 0,
        };

        let invoke_op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(stellar_xdr::ContractId(
                        stellar_xdr::Hash(source_bytes),
                    )),
                    function_name: stellar_xdr::ScSymbol("test".try_into().unwrap()),
                    args: Default::default(),
                }),
                auth: Default::default(),
            }),
        };

        // fee = inclusion_fee + resource_fee (resource_fee=0 so fee=inclusion_fee)
        let tx = Transaction {
            source_account: source,
            fee: inclusion_fee as u32,
            seq_num: stellar_xdr::SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![invoke_op].try_into().unwrap(),
            ext: TransactionExt::V1(soroban_data),
        };

        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        })
    }

    /// Default Soroban ledger-wide resource limits matching stellar-core test config.
    fn default_soroban_limits() -> Resource {
        Resource::soroban_ledger_limits(
            1000, // tx_count (mLedgerMaxTxCount)
            LEDGER_MAX_INSTRUCTIONS,
            i64::MAX,  // tx_size_bytes (no effective byte limit by default)
            1_000_000, // read_bytes (mLedgerMaxDiskReadBytes)
            100_000,   // write_bytes (mLedgerMaxWriteBytes)
            3_000,     // read_ledger_entries (mLedgerMaxDiskReadEntries)
            2_000,     // write_ledger_entries (mLedgerMaxWriteLedgerEntries)
        )
    }

    /// Create a TransactionQueue configured for parallel Soroban phase building
    /// with the given resource limits for selection.
    fn make_parallel_queue(
        soroban_limit: Resource,
        min_stage: u32,
        max_stage: u32,
    ) -> TransactionQueue {
        let config = TxQueueConfig {
            max_size: 1000,
            ledger_max_instructions: LEDGER_MAX_INSTRUCTIONS,
            ledger_max_dependent_tx_clusters: CLUSTER_COUNT,
            soroban_phase_min_stage_count: min_stage,
            soroban_phase_max_stage_count: max_stage,
            validate_signatures: false,
            validate_bounds: false,
            max_soroban_bytes: None,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);
        queue.update_soroban_selection_limits(soroban_limit);
        queue.update_validation_context(
            0,
            0,
            PROTOCOL_VERSION,
            100,
            5_000_000,
            0,
            std::time::Duration::from_secs(5),
        );
        #[cfg(test)]
        queue.set_skip_fee_balance_check(true);
        queue
    }

    /// Run a test with both variable (min=1, max=4) and fixed (min=4, max=4) stage counts.
    fn run_both<F>(f: F)
    where
        F: Fn(u32, u32),
    {
        f(1, STAGE_COUNT);
        f(STAGE_COUNT, STAGE_COUNT);
    }

    /// Extract the Soroban phase shape from a GeneralizedTransactionSet.
    /// Returns (num_stages, clusters_per_stage, txs_per_cluster) for uniform shapes.
    fn extract_soroban_phase(
        gen_tx_set: &GeneralizedTransactionSet,
    ) -> &stellar_xdr::ParallelTxsComponent {
        match gen_tx_set {
            GeneralizedTransactionSet::V1(v1) => {
                // Phase 1 is the Soroban phase
                let phase = &v1.phases[1];
                match phase {
                    stellar_xdr::TransactionPhase::V1(component) => component,
                    _ => panic!("expected V1 soroban phase"),
                }
            }
        }
    }

    /// Validate that the Soroban phase has the expected uniform shape.
    fn validate_phase_shape(
        gen_tx_set: &GeneralizedTransactionSet,
        expected_stages: usize,
        expected_clusters_per_stage: usize,
        expected_txs_per_cluster: usize,
    ) {
        let component = extract_soroban_phase(gen_tx_set);
        let stages = &component.execution_stages;

        assert_eq!(
            stages.len(),
            expected_stages,
            "expected {} stages, got {}",
            expected_stages,
            stages.len()
        );
        for (i, stage) in stages.iter().enumerate() {
            assert_eq!(
                stage.0.len(),
                expected_clusters_per_stage,
                "stage {}: expected {} clusters, got {}",
                i,
                expected_clusters_per_stage,
                stage.0.len()
            );
            for (j, cluster) in stage.0.iter().enumerate() {
                assert_eq!(
                    cluster.0.len(),
                    expected_txs_per_cluster,
                    "stage {} cluster {}: expected {} txs, got {}",
                    i,
                    j,
                    expected_txs_per_cluster,
                    cluster.0.len()
                );
            }
        }
    }

    /// Extract the base fee from the Soroban phase.
    fn extract_phase_base_fee(gen_tx_set: &GeneralizedTransactionSet) -> Option<i64> {
        extract_soroban_phase(gen_tx_set).base_fee
    }

    /// Count total transactions in the Soroban phase.
    fn count_soroban_txs(gen_tx_set: &GeneralizedTransactionSet) -> usize {
        let component = extract_soroban_phase(gen_tx_set);
        component
            .execution_stages
            .iter()
            .flat_map(|stage| stage.0.iter())
            .flat_map(|cluster| cluster.0.iter())
            .count()
    }

    /// #3680 regression: the parallel-Soroban tx-set builder must read the LIVE
    /// LCL SorobanNetworkConfig limits (refreshed via
    /// `update_soroban_parallel_limits` on ledger close), NOT a `TxQueueConfig`
    /// frozen at construction.
    ///
    /// In production the app constructs `TxQueueConfig` with
    /// `ledger_max_instructions = 0` / `ledger_max_dependent_tx_clusters = 0`
    /// (the `..Default::default()` values), so the `use_parallel` gate was
    /// permanently false and henyey never built a parallel Soroban phase after a
    /// Soroban ConfigUpgrade raised the live limits.
    ///
    /// Parity: stellar-core builds the surge-priced parallel Soroban phase from
    /// `getLastClosedSorobanNetworkConfig()` (`TxSetFrame.cpp:606-608`), reading
    /// `ledgerMaxInstructions()` / `ledgerMaxDependentTxClusters()` live.
    ///
    /// This test builds a queue whose *config* parallel limits are 0/0 (prod
    /// default) and then drives a ledger-close-style refresh with raised live
    /// limits. Pre-fix the phase collapses to a single sequential cluster;
    /// post-fix it fans out into multiple parallel clusters.
    #[test]
    fn test_parallel_soroban_limits_use_live_config_not_frozen_3680() {
        // Config parallel limits are the production defaults (0/0): parallel
        // building is disabled at construction time.
        let config = TxQueueConfig {
            max_size: 1000,
            ledger_max_instructions: 0,
            ledger_max_dependent_tx_clusters: 0,
            soroban_phase_min_stage_count: STAGE_COUNT,
            soroban_phase_max_stage_count: STAGE_COUNT,
            validate_signatures: false,
            validate_bounds: false,
            max_soroban_bytes: None,
            ..Default::default()
        };
        assert_eq!(config.ledger_max_instructions, 0);
        assert_eq!(config.ledger_max_dependent_tx_clusters, 0);

        let queue = TransactionQueue::new(config);
        queue.update_soroban_selection_limits(default_soroban_limits());
        queue.update_validation_context(
            0,
            0,
            PROTOCOL_VERSION,
            100,
            5_000_000,
            0,
            std::time::Duration::from_secs(5),
        );
        queue.set_skip_fee_balance_check(true);

        // Simulate a ledger close carrying the LIVE (raised) Soroban limits,
        // exactly as `App::update_herder_soroban_limits` does on every close.
        queue.update_soroban_parallel_limits(LEDGER_MAX_INSTRUCTIONS, CLUSTER_COUNT);

        // Effective accessors must now report the LIVE values, overriding the
        // frozen 0/0 config.
        assert_eq!(
            queue.effective_ledger_max_instructions(),
            LEDGER_MAX_INSTRUCTIONS
        );
        assert_eq!(
            queue.effective_ledger_max_dependent_tx_clusters(),
            CLUSTER_COUNT
        );

        // Add 32 Soroban txs, each using an equal slice of the per-stage
        // instruction budget so they fan out across all clusters/stages.
        let total_txs = (STAGE_COUNT * CLUSTER_COUNT) as i32;
        let per_tx_instructions = (LEDGER_MAX_INSTRUCTIONS as u32) / (STAGE_COUNT * CLUSTER_COUNT);
        let mut account_id = 0u32;
        for i in 0..total_txs {
            let tx = make_resource_limit_tx(
                &mut account_id,
                per_tx_instructions,
                &[4 * i, 4 * i + 1],
                &[4 * i + 2, 4 * i + 3],
                100 + i as i64,
                100, // small read bytes — not the bottleneck
                100,
            );
            assert_eq!(queue.try_add(tx), TxQueueResult::Added, "tx {} added", i);
        }

        let tx_set = queue.build_generalized_tx_set(Hash256::ZERO, 1000);
        let gen_tx_set = tx_set.generalized_tx_set().unwrap().clone();
        let component = extract_soroban_phase(&gen_tx_set);

        // All 32 txs must survive regardless of path.
        assert_eq!(count_soroban_txs(&gen_tx_set), total_txs as usize);

        // The load-bearing assertion: with the live limits applied, the phase
        // must be PARALLEL — multiple clusters within a stage. Pre-fix (frozen
        // 0/0 config) the else-branch produced a single cluster holding every
        // tx, so the max cluster count per stage would be 1.
        let max_clusters_per_stage = component
            .execution_stages
            .iter()
            .map(|stage| stage.0.len())
            .max()
            .unwrap_or(0);
        assert!(
            max_clusters_per_stage > 1,
            "expected a parallel Soroban phase (>1 cluster per stage) once live \
             limits are applied, got {} cluster(s) — the builder is still reading \
             the frozen 0/0 TxQueueConfig instead of the live LCL config (#3680)",
            max_clusters_per_stage
        );
    }

    /// Run a resource-limit scenario: create 32 TXs, add to queue, build tx set,
    /// validate shape and base fee.
    #[allow(clippy::too_many_arguments)]
    fn run_resource_limit_scenario(
        soroban_limit: Resource,
        min_stage: u32,
        max_stage: u32,
        make_tx: impl Fn(&mut u32, i32) -> TransactionEnvelope,
        expected_stages: usize,
        expected_clusters: usize,
        expected_txs_per_cluster: usize,
        expected_base_fee: i64,
    ) {
        let queue = make_parallel_queue(soroban_limit, min_stage, max_stage);

        let mut account_id = 0u32;
        let total_txs = (STAGE_COUNT * CLUSTER_COUNT) as i32;
        for i in 0..total_txs {
            let tx = make_tx(&mut account_id, i);
            let result = queue.try_add(tx);
            assert_eq!(
                result,
                TxQueueResult::Added,
                "tx {} should be added, got {:?}",
                i,
                result
            );
        }

        let expected_survivor_count =
            expected_stages * expected_clusters * expected_txs_per_cluster;
        let max_ops = 1000;
        let _tx_set = queue.build_generalized_tx_set(Hash256::ZERO, max_ops);
        let gen_tx_set = _tx_set.generalized_tx_set().unwrap().clone();

        validate_phase_shape(
            &gen_tx_set,
            expected_stages,
            expected_clusters,
            expected_txs_per_cluster,
        );
        assert_eq!(
            count_soroban_txs(&gen_tx_set),
            expected_survivor_count,
            "expected {} survivors",
            expected_survivor_count
        );
        assert_eq!(
            extract_phase_base_fee(&gen_tx_set),
            Some(expected_base_fee),
            "expected base fee {}",
            expected_base_fee
        );
    }

    // ---- Resource-limit scenarios ----
    // Ports stellar-core TxSetTests.cpp:2727-2863

    #[test]
    fn test_parity_resource_limit_read_bytes() {
        // Each TX uses 100KB read bytes. Ledger max = 1MB → 10 fit.
        // 32 TXs with fees 100..131, top 10 survive (fees 122..131), base fee = 122.
        run_both(|min, max| {
            let limits = default_soroban_limits();
            run_resource_limit_scenario(
                limits,
                min,
                max,
                |account_id, i| {
                    make_resource_limit_tx(
                        account_id,
                        1_000_000,
                        &[4 * i, 4 * i + 1],
                        &[4 * i + 2, 4 * i + 3],
                        100 + i as i64,
                        100_000, // 100KB read bytes
                        100,     // default write bytes
                    )
                },
                1,
                1,
                10,
                100 + (STAGE_COUNT * CLUSTER_COUNT) as i64 - 10,
            );
        });
    }

    #[test]
    fn test_parity_resource_limit_read_entries() {
        // stellar-core sets mTxMaxDiskReadEntries=43 and mLedgerMaxDiskReadEntries=43.
        // However, at protocol v23+ soroban_disk_read_entries() only counts non-Soroban keys.
        // Our test TXs use ContractData keys exclusively, so per-TX ReadLedgerEntries = 0.
        // The actual bottleneck is disk_read_bytes (100KB/TX, 1MB ledger max → 10 fit).
        // We match stellar-core by also setting read_entries=43 in our limits, verifying the
        // same outcome.
        run_both(|min, max| {
            let mut limits = default_soroban_limits();
            limits.set_val(ResourceType::ReadLedgerEntries, 43);

            // Verify our understanding: ContractData keys produce 0 disk read entries at v23+.
            {
                let mut id = 0u32;
                let tx =
                    make_resource_limit_tx(&mut id, 1_000_000, &[0, 1], &[2, 3], 100, 100_000, 100);
                let frame =
                    henyey_tx::TransactionFrame::from_owned_with_network(tx, NetworkId::testnet());
                let resources = frame.resources(false, PROTOCOL_VERSION);
                assert_eq!(
                    resources.get_val(ResourceType::ReadLedgerEntries),
                    0,
                    "ContractData keys should produce 0 disk read entries at protocol v23+"
                );
            }

            run_resource_limit_scenario(
                limits,
                min,
                max,
                |account_id, i| {
                    make_resource_limit_tx(
                        account_id,
                        1_000_000,
                        &[4 * i, 4 * i + 1],
                        &[4 * i + 2, 4 * i + 3],
                        100 + i as i64,
                        100_000,
                        100,
                    )
                },
                1,
                1,
                10,
                100 + (STAGE_COUNT * CLUSTER_COUNT) as i64 - 10,
            );
        });
    }

    #[test]
    fn test_parity_resource_limit_write_bytes() {
        // Each TX uses 10KB write bytes. Ledger max = 100KB → 10 fit.
        run_both(|min, max| {
            let limits = default_soroban_limits();
            run_resource_limit_scenario(
                limits,
                min,
                max,
                |account_id, i| {
                    make_resource_limit_tx(
                        account_id,
                        1_000_000,
                        &[4 * i, 4 * i + 1],
                        &[4 * i + 2, 4 * i + 3],
                        100 + i as i64,
                        100,    // default read bytes
                        10_000, // 10KB write bytes
                    )
                },
                1,
                1,
                10,
                100 + (STAGE_COUNT * CLUSTER_COUNT) as i64 - 10,
            );
        });
    }

    #[test]
    fn test_parity_resource_limit_write_entries() {
        // stellar-core sets mTxMaxWriteLedgerEntries=21, mLedgerMaxWriteLedgerEntries=21.
        // Each TX has 2 RW keys → 2 write entries. 21/2 = 10 fit.
        run_both(|min, max| {
            let mut limits = default_soroban_limits();
            limits.set_val(ResourceType::WriteLedgerEntries, 21);
            run_resource_limit_scenario(
                limits,
                min,
                max,
                |account_id, i| {
                    make_resource_limit_tx(
                        account_id,
                        1_000_000,
                        &[4 * i, 4 * i + 1],
                        &[4 * i + 2, 4 * i + 3],
                        100 + i as i64,
                        1_000,
                        100,
                    )
                },
                1,
                1,
                10,
                100 + (STAGE_COUNT * CLUSTER_COUNT) as i64 - 10,
            );
        });
    }

    #[test]
    fn test_parity_resource_limit_tx_size() {
        // stellar-core sets mLedgerMaxTransactionsSizeBytes = 11 * single_tx_size - 1.
        // This means only 10 TXs fit. We compute actual XDR size at runtime.
        // Note: this tests the TxByteSize dimension of Resource, not max_soroban_bytes config.
        run_both(|min, max| {
            // First, compute the XDR size of one test TX.
            let mut id = 0u32;
            let sample_tx =
                make_resource_limit_tx(&mut id, 1_000_000, &[0, 1], &[2, 3], 100, 1_000, 100);
            let tx_size = sample_tx.to_xdr(stellar_xdr::Limits::none()).unwrap().len() as i64;

            let mut limits = default_soroban_limits();
            limits.set_val(ResourceType::TxByteSize, 11 * tx_size - 1);

            run_resource_limit_scenario(
                limits,
                min,
                max,
                |account_id, i| {
                    make_resource_limit_tx(
                        account_id,
                        1_000_000,
                        &[4 * i, 4 * i + 1],
                        &[4 * i + 2, 4 * i + 3],
                        100 + i as i64,
                        1_000,
                        100,
                    )
                },
                1,
                1,
                10,
                100 + (STAGE_COUNT * CLUSTER_COUNT) as i64 - 10,
            );
        });
    }

    #[test]
    fn test_parity_resource_limit_tx_count() {
        // stellar-core sets mLedgerMaxTxCount = 5.
        // Operations dimension limits to 5 TXs (each has 1 op).
        // 32 TXs with fees 100..131, top 5 survive (fees 127..131), base fee = 127.
        run_both(|min, max| {
            let mut limits = default_soroban_limits();
            limits.set_val(ResourceType::Operations, 5);
            run_resource_limit_scenario(
                limits,
                min,
                max,
                |account_id, i| {
                    make_resource_limit_tx(
                        account_id,
                        1_000_000,
                        &[4 * i, 4 * i + 1],
                        &[4 * i + 2, 4 * i + 3],
                        100 + i as i64,
                        1_000,
                        100,
                    )
                },
                1,
                1,
                5,
                100 + (STAGE_COUNT * CLUSTER_COUNT) as i64 - 5,
            );
        });
    }
}

#[cfg(test)]
mod eviction_queue_tests {
    use super::*;
    use henyey_common::types::Hash256;
    use stellar_xdr::*;

    fn make_test_envelope(fee: u32, ops: usize) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let operations: Vec<Operation> = (0..ops)
            .map(|_| Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([255u8; 32]))),
                    starting_balance: 1000000000,
                }),
            })
            .collect();
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn set_source(envelope: &mut TransactionEnvelope, seed: u8) {
        let source = MuxedAccount::Ed25519(Uint256([seed; 32]));
        match envelope {
            TransactionEnvelope::TxV0(env) => {
                env.tx.source_account_ed25519 = Uint256([seed; 32]);
            }
            TransactionEnvelope::Tx(env) => {
                env.tx.source_account = source;
            }
            TransactionEnvelope::TxFeeBump(env) => match &mut env.tx.inner_tx {
                stellar_xdr::FeeBumpTransactionInnerTx::Tx(inner) => {
                    inner.tx.source_account = source;
                }
            },
        }
    }

    fn full_hash(envelope: &TransactionEnvelope) -> Hash256 {
        Hash256::hash_xdr(envelope)
    }

    fn make_eviction_test_queue() -> TransactionQueue {
        TransactionQueue::new(TxQueueConfig {
            max_size: 100,
            max_age_secs: 300,
            max_queue_ops: Some(50),
            max_queue_dex_ops: Some(20),
            max_queue_classic_bytes: None,
            ..Default::default()
        })
    }

    fn make_soroban_envelope_with_resources(fee: u32, instructions: u32) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([0u8; 32]))),
                    function_name: ScSymbol("test".try_into().unwrap()),
                    args: VecM::default(),
                }),
                auth: VecM::default(),
            }),
        };
        let resources = SorobanResources {
            footprint: LedgerFootprint {
                read_only: VecM::default(),
                read_write: VecM::default(),
            },
            instructions,
            disk_read_bytes: 0,
            write_bytes: 0,
        };
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V1(SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources,
                resource_fee: 0,
            }),
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    /// After adding several txs via try_add, the persistent eviction queues
    /// should match a cold rebuild from by_hash.
    #[test]
    fn test_persistent_eviction_queues_consistent_after_inserts() {
        let queue = make_eviction_test_queue();

        for i in 1..=5u8 {
            let mut tx = make_test_envelope(100 + i as u32, 1);
            set_source(&mut tx, i);
            assert_eq!(queue.try_add(tx), TxQueueResult::Added);
        }

        let store = queue.store.read();
        store.assert_consistent();
        store.assert_eviction_queues_consistent(0);
    }

    /// After ban() removes txs, eviction queues should stay consistent.
    #[test]
    fn test_persistent_eviction_queues_consistent_after_ban() {
        let queue = make_eviction_test_queue();

        let mut tx1 = make_test_envelope(200, 1);
        set_source(&mut tx1, 1);
        let hash1 = Hash256::hash_xdr(&tx1);
        let mut tx2 = make_test_envelope(300, 1);
        set_source(&mut tx2, 2);
        let mut tx3 = make_test_envelope(400, 1);
        set_source(&mut tx3, 3);

        queue.try_add(tx1);
        queue.try_add(tx2);
        queue.try_add(tx3);
        assert_eq!(queue.len(), 3);

        queue.ban(&[hash1]);
        assert_eq!(queue.len(), 2);

        let store = queue.store.read();
        store.assert_consistent();
        store.assert_eviction_queues_consistent(0);
    }

    /// After shift() auto-bans stale txs, eviction queues should be invalidated
    /// and rebuilt consistently on next access.
    #[test]
    fn test_persistent_eviction_queues_invalidated_after_shift() {
        let queue = TransactionQueue::with_ban_depth(
            TxQueueConfig {
                max_size: 100,
                max_age_secs: 300,
                max_queue_ops: Some(50),
                max_queue_dex_ops: Some(20),
                max_queue_classic_bytes: None,
                ..Default::default()
            },
            3,
        );

        let mut tx1 = make_test_envelope(200, 1);
        set_source(&mut tx1, 1);
        queue.try_add(tx1);

        let mut tx2 = make_test_envelope(300, 1);
        set_source(&mut tx2, 2);
        queue.try_add(tx2);

        queue.shift();

        let store = queue.store.read();
        assert!(
            store.classic_eviction_queue.is_none(),
            "classic queue should be invalidated after shift"
        );
        assert!(
            store.global_ops_queue.is_none(),
            "global ops queue should be invalidated after shift"
        );
    }

    /// After clear(), eviction queues should be None, seed regenerated,
    /// and eviction thresholds reset so low-fee txs are accepted again.
    #[test]
    fn test_persistent_eviction_queues_cleared() {
        // Use a small queue that will trigger evictions and set thresholds.
        let queue = TransactionQueue::new(TxQueueConfig {
            max_size: 3,
            max_age_secs: 300,
            max_queue_ops: None,
            max_queue_dex_ops: None,
            max_queue_classic_bytes: None,
            ..Default::default()
        });

        // Fill queue to capacity.
        for i in 1..=3u8 {
            let mut tx = make_test_envelope(200 + i as u32, 1);
            set_source(&mut tx, i);
            assert_eq!(queue.try_add(tx), TxQueueResult::Added);
        }
        assert_eq!(queue.len(), 3);

        // A low-fee tx should be rejected (queue is full, fee too low to evict).
        let mut low_fee_tx = make_test_envelope(100, 1);
        set_source(&mut low_fee_tx, 10);
        let result = queue.try_add(low_fee_tx.clone());
        assert!(
            result == TxQueueResult::FeeTooLow || result == TxQueueResult::QueueFull,
            "expected rejection, got {:?}",
            result
        );

        // Clear should reset everything.
        queue.clear();

        let store = queue.store.read();
        assert!(store.classic_eviction_queue.is_none());
        assert!(store.soroban_eviction_queue.is_none());
        assert!(store.global_ops_queue.is_none());
        assert_eq!(store.by_hash.len(), 0);
        drop(store);

        // After clear, the same low-fee tx should be accepted (thresholds reset, queue empty).
        assert_eq!(queue.try_add(low_fee_tx), TxQueueResult::Added);
    }

    /// After remove_applied removes txs, eviction queues stay consistent.
    #[test]
    fn test_persistent_eviction_queues_consistent_after_remove_applied() {
        let queue = make_eviction_test_queue();

        let mut tx1 = make_test_envelope(200, 1);
        set_source(&mut tx1, 1);
        let mut tx2 = make_test_envelope(300, 1);
        set_source(&mut tx2, 2);
        let mut tx3 = make_test_envelope(400, 1);
        set_source(&mut tx3, 3);

        queue.try_add(tx1.clone());
        queue.try_add(tx2.clone());
        queue.try_add(tx3.clone());
        assert_eq!(queue.len(), 3);

        queue.remove_applied(&[(tx1, 1)]);
        assert_eq!(queue.len(), 2);

        let store = queue.store.read();
        store.assert_consistent();
        store.assert_eviction_queues_consistent(0);
    }

    /// After update_soroban_resource_limits expands limits, a previously-rejected
    /// Soroban tx (FeeTooLow due to stale thresholds) should now be accepted.
    /// Regression test for the bug caught during #1813 review.
    #[test]
    fn test_soroban_fee_too_low_cleared_after_limit_expansion() {
        use henyey_common::{ResourceType, NUM_SOROBAN_TX_RESOURCES};

        // Start with tight Soroban limits: only 100 instructions allowed total.
        let mut initial_limit = Resource::new(vec![i64::MAX; NUM_SOROBAN_TX_RESOURCES]);
        initial_limit.set_val(ResourceType::Instructions, 100);
        let config = TxQueueConfig {
            max_queue_soroban_resources: Some(initial_limit),
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Fill to capacity: tx with 80 instructions uses most of the budget.
        let mut tx1 = make_soroban_envelope_with_resources(4000, 80);
        set_source(&mut tx1, 91);
        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);

        // tx2 also needs 80 instructions — evicts tx1 (higher fee wins).
        let mut tx2 = make_soroban_envelope_with_resources(8000, 80);
        set_source(&mut tx2, 92);
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);
        assert_eq!(queue.len(), 1, "tx1 should have been evicted");

        // tx3 has fee=4000 which equals the evicted tx1's fee — should be
        // rejected as FeeTooLow because cached thresholds remember the eviction.
        let mut tx3 = make_soroban_envelope_with_resources(4000, 80);
        set_source(&mut tx3, 93);
        assert_eq!(
            queue.try_add(tx3.clone()),
            TxQueueResult::FeeTooLow,
            "tx3 should be rejected before limit expansion"
        );

        // Expand limits: now 200 instructions allowed — both tx2 and tx3 can fit.
        let mut expanded_limit = Resource::new(vec![i64::MAX; NUM_SOROBAN_TX_RESOURCES]);
        expanded_limit.set_val(ResourceType::Instructions, 200);
        queue.update_soroban_resource_limits(expanded_limit);

        // After limit expansion, tx3 should now be accepted (thresholds were reset).
        assert_eq!(
            queue.try_add(tx3),
            TxQueueResult::Added,
            "tx3 should be accepted after Soroban limit expansion resets thresholds"
        );
        assert_eq!(queue.len(), 2);
    }

    /// After update_soroban_resource_limits, the soroban eviction queue
    /// should be invalidated (set to None) for lazy rebuild.
    #[test]
    fn test_soroban_queue_invalidated_on_limit_update() {
        let queue = TransactionQueue::new(TxQueueConfig {
            max_size: 100,
            max_age_secs: 300,
            max_queue_soroban_resources: Some(Resource::new(vec![
                100, 100, 100, 100, 100, 100, 100,
            ])),
            ..Default::default()
        });

        {
            let mut store = queue.store.write();
            store.ensure_soroban_queue(Resource::new(vec![100, 100, 100, 100, 100, 100, 100]), 0);
            assert!(store.soroban_eviction_queue.is_some());
        }

        queue
            .update_soroban_resource_limits(Resource::new(vec![200, 200, 200, 200, 200, 200, 200]));

        let store = queue.store.read();
        assert!(
            store.soroban_eviction_queue.is_none(),
            "soroban queue should be invalidated after limit update"
        );
    }

    /// After ban() removes a tx that was the eviction-threshold setter,
    /// cached thresholds should be reset so a tx at the same fee level
    /// is no longer rejected as FeeTooLow.
    #[test]
    fn test_ban_resets_eviction_thresholds() {
        // Queue with ops limit=2 — capacity for 2 ops.
        let queue = TransactionQueue::new(TxQueueConfig {
            max_size: 100,
            max_age_secs: 300,
            max_queue_ops: Some(2),
            ..Default::default()
        });

        // Fill queue with 2 txs.
        let mut tx1 = make_test_envelope(1000, 1);
        set_source(&mut tx1, 101);
        let mut tx2 = make_test_envelope(2000, 1);
        set_source(&mut tx2, 102);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);

        // Add a higher-fee tx that evicts tx1, setting the eviction threshold.
        let mut tx3 = make_test_envelope(3000, 1);
        set_source(&mut tx3, 103);
        let hash3 = full_hash(&tx3);
        assert_eq!(queue.try_add(tx3), TxQueueResult::Added);
        assert_eq!(queue.len(), 2); // tx1 was evicted

        // Now a tx below the cached threshold should be rejected as FeeTooLow.
        let mut tx4_low = make_test_envelope(500, 1);
        set_source(&mut tx4_low, 104);
        assert_eq!(queue.try_add(tx4_low.clone()), TxQueueResult::FeeTooLow);

        // Ban tx3 — frees a slot and resets thresholds.
        queue.ban(&[hash3]);
        assert_eq!(queue.len(), 1);

        // After ban + threshold reset, the previously-rejected tx should succeed.
        assert_eq!(
            queue.try_add(tx4_low),
            TxQueueResult::Added,
            "lower-fee tx should be accepted after ban resets thresholds"
        );
    }

    /// After remove_applied() removes a tx, cached thresholds should be
    /// reset so subsequent try_add is not rejected with stale FeeTooLow.
    #[test]
    fn test_remove_applied_resets_eviction_thresholds() {
        // Queue with ops limit=2.
        let queue = TransactionQueue::new(TxQueueConfig {
            max_size: 100,
            max_age_secs: 300,
            max_queue_ops: Some(2),
            ..Default::default()
        });

        // Fill queue with 2 txs.
        let mut tx1 = make_test_envelope(1000, 1);
        set_source(&mut tx1, 111);
        let mut tx2 = make_test_envelope(2000, 1);
        set_source(&mut tx2, 112);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);

        // Add a higher-fee tx that evicts tx1, setting the eviction threshold.
        let mut tx3 = make_test_envelope(3000, 1);
        set_source(&mut tx3, 113);
        assert_eq!(queue.try_add(tx3.clone()), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        // tx below cached threshold should be rejected.
        let mut tx4_low = make_test_envelope(500, 1);
        set_source(&mut tx4_low, 114);
        assert_eq!(queue.try_add(tx4_low.clone()), TxQueueResult::FeeTooLow);

        // Remove tx3 as applied — frees a slot and resets thresholds.
        queue.remove_applied(&[(tx3, 1)]);
        assert_eq!(queue.len(), 1);

        // After remove_applied + threshold reset, the previously-rejected tx should succeed.
        assert_eq!(
            queue.try_add(tx4_low),
            TxQueueResult::Added,
            "lower-fee tx should be accepted after remove_applied resets thresholds"
        );
    }

    /// Admission-path eviction (try_add) should preserve cached thresholds so
    /// that subsequent low-fee submissions are still fast-rejected.
    #[test]
    fn test_try_add_eviction_preserves_thresholds() {
        // Queue with ops limit=2.
        let queue = TransactionQueue::new(TxQueueConfig {
            max_size: 100,
            max_age_secs: 300,
            max_queue_ops: Some(2),
            ..Default::default()
        });

        // Fill queue with 2 txs.
        let mut tx1 = make_test_envelope(1000, 1);
        set_source(&mut tx1, 121);
        let mut tx2 = make_test_envelope(2000, 1);
        set_source(&mut tx2, 122);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);

        // Add a higher-fee tx that evicts tx1, setting the eviction threshold.
        let mut tx3 = make_test_envelope(3000, 1);
        set_source(&mut tx3, 123);
        assert_eq!(queue.try_add(tx3), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        // The eviction threshold from tx1's eviction should still be cached.
        // A tx below that threshold should be rejected.
        let mut tx4_low = make_test_envelope(500, 1);
        set_source(&mut tx4_low, 124);
        assert_eq!(
            queue.try_add(tx4_low),
            TxQueueResult::FeeTooLow,
            "admission-path eviction should preserve cached thresholds"
        );
    }

    /// Banning hashes that are NOT in the queue should not touch thresholds.
    #[test]
    fn test_ban_noop_preserves_thresholds() {
        // Queue with ops limit=2.
        let queue = TransactionQueue::new(TxQueueConfig {
            max_size: 100,
            max_age_secs: 300,
            max_queue_ops: Some(2),
            ..Default::default()
        });

        // Fill queue with 2 txs.
        let mut tx1 = make_test_envelope(1000, 1);
        set_source(&mut tx1, 131);
        let mut tx2 = make_test_envelope(2000, 1);
        set_source(&mut tx2, 132);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);

        // Add a higher-fee tx that evicts tx1, setting the eviction threshold.
        let mut tx3 = make_test_envelope(3000, 1);
        set_source(&mut tx3, 133);
        assert_eq!(queue.try_add(tx3), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        // Verify threshold is active.
        let mut tx4_low = make_test_envelope(500, 1);
        set_source(&mut tx4_low, 134);
        assert_eq!(queue.try_add(tx4_low.clone()), TxQueueResult::FeeTooLow);

        // Ban a hash that's NOT in the queue — should not reset thresholds.
        let fake_hash = Hash256::from([0xFFu8; 32]);
        queue.ban(&[fake_hash]);

        // Threshold should still be active.
        assert_eq!(
            queue.try_add(tx4_low),
            TxQueueResult::FeeTooLow,
            "banning absent hash should not reset thresholds"
        );
    }

    /// evict_expired() with actual expired txs should reset thresholds.
    #[test]
    fn test_evict_expired_resets_thresholds() {
        // Queue with ops limit=2 and very short max_age.
        let queue = TransactionQueue::new(TxQueueConfig {
            max_size: 100,
            max_age_secs: 0, // expire after >0 seconds
            max_queue_ops: Some(2),
            ..Default::default()
        });

        // Fill queue with 2 txs.
        let mut tx1 = make_test_envelope(1000, 1);
        set_source(&mut tx1, 141);
        let mut tx2 = make_test_envelope(2000, 1);
        set_source(&mut tx2, 142);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);

        // Add a higher-fee tx that evicts tx1, setting the eviction threshold.
        let mut tx3 = make_test_envelope(3000, 1);
        set_source(&mut tx3, 143);
        assert_eq!(queue.try_add(tx3), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        // Verify threshold is active.
        let mut tx4_low = make_test_envelope(500, 1);
        set_source(&mut tx4_low, 144);
        assert_eq!(queue.try_add(tx4_low.clone()), TxQueueResult::FeeTooLow);

        // Wait >1 second so is_expired() (as_secs() > 0) returns true.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        queue.evict_expired();
        assert_eq!(queue.len(), 0, "all txs should be expired");

        // Thresholds should be reset after eviction removed expired txs.
        // The low-fee tx should now succeed (queue is empty).
        assert_eq!(
            queue.try_add(tx4_low),
            TxQueueResult::Added,
            "evict_expired should reset thresholds after removing expired txs"
        );
    }

    /// evict_expired() with nothing to expire should preserve thresholds.
    #[test]
    fn test_evict_expired_noop_preserves_thresholds() {
        // Queue with ops limit=2 and long max_age.
        let queue = TransactionQueue::new(TxQueueConfig {
            max_size: 100,
            max_age_secs: 300,
            max_queue_ops: Some(2),
            ..Default::default()
        });

        // Fill queue with 2 txs.
        let mut tx1 = make_test_envelope(1000, 1);
        set_source(&mut tx1, 151);
        let mut tx2 = make_test_envelope(2000, 1);
        set_source(&mut tx2, 152);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);

        // Add a higher-fee tx that evicts tx1, setting the eviction threshold.
        let mut tx3 = make_test_envelope(3000, 1);
        set_source(&mut tx3, 153);
        assert_eq!(queue.try_add(tx3), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        // Verify threshold is active.
        let mut tx4_low = make_test_envelope(500, 1);
        set_source(&mut tx4_low, 154);
        assert_eq!(queue.try_add(tx4_low.clone()), TxQueueResult::FeeTooLow);

        // evict_expired with nothing to expire — thresholds should be preserved.
        queue.evict_expired();

        assert_eq!(
            queue.try_add(tx4_low),
            TxQueueResult::FeeTooLow,
            "evict_expired with no expired txs should preserve thresholds"
        );
    }

    /// remove_applied() with no matching queued tx should preserve thresholds.
    #[test]
    fn test_remove_applied_noop_preserves_thresholds() {
        // Queue with ops limit=2.
        let queue = TransactionQueue::new(TxQueueConfig {
            max_size: 100,
            max_age_secs: 300,
            max_queue_ops: Some(2),
            ..Default::default()
        });

        // Fill queue with 2 txs.
        let mut tx1 = make_test_envelope(1000, 1);
        set_source(&mut tx1, 161);
        let mut tx2 = make_test_envelope(2000, 1);
        set_source(&mut tx2, 162);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);

        // Add a higher-fee tx that evicts tx1, setting the eviction threshold.
        let mut tx3 = make_test_envelope(3000, 1);
        set_source(&mut tx3, 163);
        assert_eq!(queue.try_add(tx3), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        // Verify threshold is active.
        let mut tx4_low = make_test_envelope(500, 1);
        set_source(&mut tx4_low, 164);
        assert_eq!(queue.try_add(tx4_low.clone()), TxQueueResult::FeeTooLow);

        // remove_applied with a tx that's NOT in the queue — thresholds preserved.
        let mut unrelated_tx = make_test_envelope(9999, 1);
        set_source(&mut unrelated_tx, 199);
        queue.remove_applied(&[(unrelated_tx, 999)]);

        assert_eq!(
            queue.try_add(tx4_low),
            TxQueueResult::FeeTooLow,
            "remove_applied with no matching queued tx should preserve thresholds"
        );
    }
}

#[cfg(test)]
mod fee_rate_tests {
    use super::*;
    use henyey_tx::{FeeRate, InclusionFee};

    #[test]
    fn test_compute_better_fee_i64_max_saturation() {
        let rate = FeeRate::new(InclusionFee::new(i64::MAX), 1);
        let result = compute_better_fee(&rate, 2);
        assert_eq!(result, i64::MAX);
    }

    #[test]
    fn test_compute_better_fee_normal() {
        // evicted_fee=100, evicted_ops=2, tx_ops=4 → base = 200, candidate = 201
        let rate = FeeRate::new(InclusionFee::new(100), 2);
        assert_eq!(compute_better_fee(&rate, 4), 201);
    }

    #[test]
    fn test_compute_better_fee_zero_evicted_ops() {
        let rate = FeeRate::new(InclusionFee::new(100), 0);
        assert_eq!(compute_better_fee(&rate, 4), 0);
    }

    #[test]
    fn test_min_inclusion_fee_to_beat_already_better() {
        let tx = QueuedTransaction {
            envelope: Arc::new(make_dummy_envelope(200, 1)),
            hash: Hash256::from_bytes([0u8; 32]),
            total_fee: 200,
            fee_rate: FeeRate::new(InclusionFee::new(200), 1),
            fee_per_op: 200,
            received_at: std::time::Instant::now(),
            is_dex: false,
        };
        let evicted = FeeRate::new(InclusionFee::new(100), 1);
        assert_eq!(min_inclusion_fee_to_beat(Some(&evicted), &tx.fee_rate), 0);
    }

    #[test]
    fn test_min_inclusion_fee_to_beat_needs_higher() {
        let tx = QueuedTransaction {
            envelope: Arc::new(make_dummy_envelope(50, 1)),
            hash: Hash256::from_bytes([0u8; 32]),
            total_fee: 50,
            fee_rate: FeeRate::new(InclusionFee::new(50), 1),
            fee_per_op: 50,
            received_at: std::time::Instant::now(),
            is_dex: false,
        };
        let evicted = FeeRate::new(InclusionFee::new(100), 1);
        let result = min_inclusion_fee_to_beat(Some(&evicted), &tx.fee_rate);
        assert_eq!(result, 101);
    }

    #[test]
    fn test_can_replace_by_fee_sufficient() {
        let new = FeeRate::new(InclusionFee::new(1000), 1);
        let old = FeeRate::new(InclusionFee::new(100), 1);
        assert!(can_replace_by_fee(&new, &old).is_ok());
    }

    #[test]
    fn test_can_replace_by_fee_insufficient() {
        let new = FeeRate::new(InclusionFee::new(999), 1);
        let old = FeeRate::new(InclusionFee::new(100), 1);
        let result = can_replace_by_fee(&new, &old);
        assert!(result.is_err());
    }

    #[test]
    fn test_can_replace_by_fee_i64_boundary() {
        let large = i64::MAX / 20;
        let new = FeeRate::new(InclusionFee::new(large * 10), 1);
        let old = FeeRate::new(InclusionFee::new(large), 1);
        assert!(can_replace_by_fee(&new, &old).is_ok());
    }

    fn make_dummy_envelope(fee: u32, ops: u32) -> TransactionEnvelope {
        use stellar_xdr::*;
        let mut operations = Vec::new();
        for _ in 0..ops {
            operations.push(Operation {
                source_account: None,
                body: OperationBody::Inflation,
            });
        }
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: Transaction {
                source_account: MuxedAccount::Ed25519(Uint256([0; 32])),
                fee,
                seq_num: SequenceNumber(1),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: operations.try_into().unwrap(),
                ext: TransactionExt::V0,
            },
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0; 4]),
                signature: Signature(vec![0; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }
}

#[cfg(test)]
mod broadcast_visitor_tests {
    use super::*;
    use stellar_xdr::*;

    fn make_test_envelope(fee: u32, ops: usize) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let operations: Vec<Operation> = (0..ops)
            .map(|_| Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([255u8; 32]))),
                    starting_balance: 1000000000,
                }),
            })
            .collect();
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    fn set_source(envelope: &mut TransactionEnvelope, seed: u8) {
        match envelope {
            TransactionEnvelope::Tx(ref mut env) => {
                env.tx.source_account = MuxedAccount::Ed25519(Uint256([seed; 32]));
            }
            _ => panic!("Expected Tx variant"),
        }
    }

    /// Create a DEX tx envelope containing a ManageSellOffer operation.
    fn make_dex_envelope(fee: u32, ops: usize) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let operations: Vec<Operation> = (0..ops)
            .map(|_| Operation {
                source_account: None,
                body: OperationBody::ManageSellOffer(ManageSellOfferOp {
                    selling: Asset::Native,
                    buying: Asset::CreditAlphanum4(AlphaNum4 {
                        asset_code: AssetCode4(*b"USD\0"),
                        issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                    }),
                    amount: 100,
                    price: Price { n: 1, d: 1 },
                    offer_id: 0,
                }),
            })
            .collect();
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: operations.try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    /// Helper: visit all candidates as Processed, collecting them.
    fn visit_all_processed(
        queue: &TransactionQueue,
        ops_budget: usize,
        dex_ops_budget: Option<usize>,
    ) -> (Vec<BroadcastCandidate>, BroadcastBudget) {
        let mut budget = BroadcastBudget {
            ops_remaining: ops_budget,
            dex_ops_remaining: dex_ops_budget,
        };
        let mut results = Vec::new();
        queue.broadcast_with_visitor(&mut budget, |candidate| {
            results.push(candidate.clone());
            BroadcastVisitResult::Processed
        });
        (results, budget)
    }

    #[test]
    fn test_broadcast_visitor_priority_order() {
        let config = TxQueueConfig {
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx_low = make_test_envelope(100, 1);
        set_source(&mut tx_low, 1);
        let mut tx_mid = make_test_envelope(200, 1);
        set_source(&mut tx_mid, 2);
        let mut tx_high = make_test_envelope(300, 1);
        set_source(&mut tx_high, 3);

        assert_eq!(queue.try_add(tx_low.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_mid.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_high.clone()), TxQueueResult::Added);

        let (entries, budget) = visit_all_processed(&queue, 100, None);
        assert_eq!(entries.len(), 3);
        assert_eq!(budget.ops_remaining, 97);

        let hash_high = Hash256::hash_xdr(&tx_high);
        let hash_mid = Hash256::hash_xdr(&tx_mid);
        let hash_low = Hash256::hash_xdr(&tx_low);
        assert_eq!(entries[0].hash, hash_high);
        assert_eq!(entries[1].hash, hash_mid);
        assert_eq!(entries[2].hash, hash_low);
    }

    #[test]
    fn test_broadcast_visitor_ops_budget_cap() {
        let config = TxQueueConfig {
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx1 = make_test_envelope(600, 2);
        set_source(&mut tx1, 1);
        let mut tx2 = make_test_envelope(400, 2);
        set_source(&mut tx2, 2);
        let mut tx3 = make_test_envelope(200, 2);
        set_source(&mut tx3, 3);

        queue.try_add(tx1);
        queue.try_add(tx2);
        queue.try_add(tx3);

        // Budget of 3: only 1 tx fits (each has 2 ops)
        let (entries, budget) = visit_all_processed(&queue, 3, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(budget.ops_remaining, 1);

        // Rebroadcast to repopulate flood queue for next call
        queue.rebroadcast();

        // Budget of 4: 2 txs fit
        let (entries, budget) = visit_all_processed(&queue, 4, None);
        assert_eq!(entries.len(), 2);
        assert_eq!(budget.ops_remaining, 0);

        // Rebroadcast again
        queue.rebroadcast();

        // Budget of 6: all 3 fit
        let (entries, budget) = visit_all_processed(&queue, 6, None);
        assert_eq!(entries.len(), 3);
        assert_eq!(budget.ops_remaining, 0);
    }

    #[test]
    fn test_broadcast_visitor_empty_queue() {
        let config = TxQueueConfig::default();
        let queue = TransactionQueue::new(config);

        let (entries, budget) = visit_all_processed(&queue, 100, None);
        assert!(entries.is_empty());
        assert_eq!(budget.ops_remaining, 100);
    }

    #[test]
    fn test_broadcast_visitor_zero_budget() {
        let config = TxQueueConfig {
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx = make_test_envelope(100, 1);
        set_source(&mut tx, 1);
        queue.try_add(tx);

        let (entries, budget) = visit_all_processed(&queue, 0, None);
        assert!(entries.is_empty());
        assert_eq!(budget.ops_remaining, 0);
    }

    #[test]
    fn test_broadcast_visitor_after_remove_applied() {
        let config = TxQueueConfig {
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx1 = make_test_envelope(300, 1);
        set_source(&mut tx1, 1);
        let mut tx2 = make_test_envelope(200, 1);
        set_source(&mut tx2, 2);

        queue.try_add(tx1.clone());
        queue.try_add(tx2.clone());
        queue.remove_applied(&[(tx1, 300)]);

        let (entries, _) = visit_all_processed(&queue, 100, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash, Hash256::hash_xdr(&tx2));
    }

    #[test]
    fn test_broadcast_visitor_dex_budget_limits() {
        let config = TxQueueConfig {
            max_size: 10,
            max_dex_ops: Some(1000),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex1 = make_dex_envelope(300, 1);
        set_source(&mut dex1, 1);
        let mut dex2 = make_dex_envelope(200, 1);
        set_source(&mut dex2, 2);
        let mut non_dex = make_test_envelope(100, 1);
        set_source(&mut non_dex, 3);

        queue.try_add(dex1.clone());
        queue.try_add(dex2.clone());
        queue.try_add(non_dex.clone());

        // DEX budget of 1: only 1 DEX tx fits, then DEX lane drops, non-DEX still traversed
        let (entries, budget) = visit_all_processed(&queue, 100, Some(1));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].hash, Hash256::hash_xdr(&dex1));
        assert!(entries[0].is_dex);
        assert_eq!(entries[1].hash, Hash256::hash_xdr(&non_dex));
        assert!(!entries[1].is_dex);
        assert_eq!(budget.ops_remaining, 98);
        assert_eq!(budget.dex_ops_remaining, Some(0));
    }

    #[test]
    fn test_broadcast_visitor_dex_budget_independent_of_queue_dex_config() {
        // With the persistent flood queue, DEX lane topology is determined by
        // max_dex_ops config (not by the caller's budget). This test verifies
        // that flood-time DEX enforcement works regardless of max_queue_dex_ops.
        let config = TxQueueConfig {
            max_size: 10,
            max_dex_ops: Some(1000),
            max_queue_dex_ops: None,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex1 = make_dex_envelope(300, 1);
        set_source(&mut dex1, 1);
        let mut dex2 = make_dex_envelope(200, 1);
        set_source(&mut dex2, 2);
        let mut non_dex = make_test_envelope(100, 1);
        set_source(&mut non_dex, 3);

        assert_eq!(queue.try_add(dex1.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(dex2), TxQueueResult::Added);
        assert_eq!(queue.try_add(non_dex.clone()), TxQueueResult::Added);

        let (entries, _) = visit_all_processed(&queue, 100, Some(1));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].hash, Hash256::hash_xdr(&dex1));
        assert_eq!(entries[1].hash, Hash256::hash_xdr(&non_dex));
    }

    #[test]
    fn test_broadcast_visitor_uses_ops_flood_limits_with_queue_byte_config() {
        let config = TxQueueConfig {
            max_size: 10,
            max_queue_classic_bytes: Some(1_000_000),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx1 = make_test_envelope(300, 1);
        set_source(&mut tx1, 1);
        let mut tx2 = make_test_envelope(200, 1);
        set_source(&mut tx2, 2);

        assert_eq!(queue.try_add(tx1.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx2.clone()), TxQueueResult::Added);

        let (entries, budget) = visit_all_processed(&queue, 1, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash, Hash256::hash_xdr(&tx1));
        assert_eq!(budget.ops_remaining, 0);
    }

    /// Sustain fix: a pending tx that ages without applying is re-marked for
    /// flooding by `shift()` at ages 2 and 3 (once per level), so a tx whose
    /// first advert was lost or starved is rescued before the 4-ledger
    /// auto-ban. Age 1 must NOT re-flood: one ledger of latency is normal at
    /// sustained max load, and pushing there amplifies flood traffic ~6x
    /// (2026-07-04 iter-15 regression: mass age-outs from the self-inflicted
    /// storm).
    #[test]
    fn test_shift_refloods_pending_tx_at_ages_two_and_three() {
        let config = TxQueueConfig {
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx = make_test_envelope(100, 1);
        set_source(&mut tx, 1);
        let hash = Hash256::hash_xdr(&tx);
        assert_eq!(queue.try_add(tx), TxQueueResult::Added);

        // First broadcast drains the flood queue (advert sent, entry erased).
        let (entries, _) = visit_all_processed(&queue, 100, None);
        assert_eq!(entries.len(), 1, "initial flood visit sees the tx");
        let (entries, _) = visit_all_processed(&queue, 100, None);
        assert!(entries.is_empty(), "flood queue drained after first visit");

        // Age 1: normal latency — must NOT re-flood.
        let shift1 = queue.shift();
        assert!(shift1.reflooded_txs.is_empty(), "age 1 must not re-flood");
        let (entries, _) = visit_all_processed(&queue, 100, None);
        assert!(entries.is_empty(), "age 1 must not re-mark");

        // Age 2: first rescue.
        let shift2 = queue.shift();
        assert_eq!(shift2.reflooded_txs.len(), 1, "age 2 must re-flood");
        let (entries, _) = visit_all_processed(&queue, 100, None);
        assert_eq!(entries.len(), 1, "age 2 re-marks the pending tx");
        assert_eq!(entries[0].hash, hash);

        // Age 3: last-chance retry, one ledger before the age-4 auto-ban.
        let shift3 = queue.shift();
        assert_eq!(shift3.reflooded_txs.len(), 1, "age 3 must re-flood");
        let (entries, _) = visit_all_processed(&queue, 100, None);
        assert_eq!(entries.len(), 1, "age 3 re-marks the pending tx");
    }

    #[test]
    fn test_broadcast_visitor_equal_fee_order_matches_fee_index() {
        let config = TxQueueConfig {
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx1 = make_test_envelope(100, 1);
        set_source(&mut tx1, 1);
        let mut tx2 = make_test_envelope(100, 1);
        set_source(&mut tx2, 2);
        let mut tx3 = make_test_envelope(100, 1);
        set_source(&mut tx3, 3);

        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx2), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx3), TxQueueResult::Added);

        let expected: Vec<_> = {
            let store = queue.store.read();
            store
                .fee_index
                .iter()
                .rev()
                .map(|entry| entry.hash)
                .collect()
        };
        let (entries, _) = visit_all_processed(&queue, 100, None);
        let actual: Vec<_> = entries.iter().map(|candidate| candidate.hash).collect();

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_broadcast_visitor_dex_lane_drop() {
        // When top DEX tx exceeds DEX budget, ALL DEX txs are dropped
        let config = TxQueueConfig {
            max_size: 10,
            max_dex_ops: Some(1000),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // DEX tx with 3 ops exceeds DEX budget of 2 → drops lane
        let mut dex_big = make_dex_envelope(900, 3);
        set_source(&mut dex_big, 1);
        let mut dex_small = make_dex_envelope(100, 1);
        set_source(&mut dex_small, 2);
        let mut non_dex = make_test_envelope(200, 1);
        set_source(&mut non_dex, 3);

        queue.try_add(dex_big);
        queue.try_add(dex_small);
        queue.try_add(non_dex.clone());

        let (entries, _) = visit_all_processed(&queue, 100, Some(2));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash, Hash256::hash_xdr(&non_dex));
        assert!(!entries[0].is_dex);
    }

    #[test]
    fn test_broadcast_visitor_dex_lane_drop_continues_non_dex() {
        let config = TxQueueConfig {
            max_size: 10,
            max_dex_ops: Some(1000),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex_high = make_dex_envelope(500, 1);
        set_source(&mut dex_high, 1);
        let mut non_dex_high = make_test_envelope(400, 1);
        set_source(&mut non_dex_high, 2);
        let mut dex_low = make_dex_envelope(300, 1);
        set_source(&mut dex_low, 3);
        let mut non_dex_low = make_test_envelope(200, 1);
        set_source(&mut non_dex_low, 4);

        queue.try_add(dex_high.clone());
        queue.try_add(non_dex_high.clone());
        queue.try_add(dex_low);
        queue.try_add(non_dex_low.clone());

        // DEX budget 1: dex_high fits, dex_low exceeds → drops DEX lane
        let (entries, _) = visit_all_processed(&queue, 100, Some(1));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].hash, Hash256::hash_xdr(&dex_high));
        assert!(entries[0].is_dex);
        assert_eq!(entries[1].hash, Hash256::hash_xdr(&non_dex_high));
        assert!(!entries[1].is_dex);
        assert_eq!(entries[2].hash, Hash256::hash_xdr(&non_dex_low));
        assert!(!entries[2].is_dex);
    }

    #[test]
    fn test_broadcast_visitor_dex_exceeds_generic_breaks() {
        let config = TxQueueConfig {
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex = make_dex_envelope(300, 3);
        set_source(&mut dex, 1);
        let mut non_dex = make_test_envelope(100, 1);
        set_source(&mut non_dex, 2);

        queue.try_add(dex);
        queue.try_add(non_dex);

        // Generic budget 2, DEX budget 10: DEX tx (3 ops) exceeds generic → break
        let (entries, _) = visit_all_processed(&queue, 2, Some(10));
        assert!(entries.is_empty());
    }

    #[test]
    fn test_broadcast_visitor_no_dex_budget_uncapped() {
        let config = TxQueueConfig {
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex1 = make_dex_envelope(300, 1);
        set_source(&mut dex1, 1);
        let mut dex2 = make_dex_envelope(200, 1);
        set_source(&mut dex2, 2);

        queue.try_add(dex1.clone());
        queue.try_add(dex2.clone());

        // No DEX budget (None): all DEX txs use generic budget only
        let (entries, budget) = visit_all_processed(&queue, 100, None);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_dex);
        assert!(entries[1].is_dex);
        assert_eq!(budget.ops_remaining, 98);
        assert_eq!(budget.dex_ops_remaining, None);
    }

    #[test]
    fn test_broadcast_visitor_returns_broadcast_candidate() {
        let config = TxQueueConfig {
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex = make_dex_envelope(200, 2);
        set_source(&mut dex, 1);
        let mut non_dex = make_test_envelope(100, 1);
        set_source(&mut non_dex, 2);

        queue.try_add(dex.clone());
        queue.try_add(non_dex.clone());

        let (entries, _) = visit_all_processed(&queue, 100, None);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].hash, Hash256::hash_xdr(&dex));
        assert_eq!(entries[0].op_count, 2);
        assert!(entries[0].is_dex);
        assert_eq!(entries[1].hash, Hash256::hash_xdr(&non_dex));
        assert_eq!(entries[1].op_count, 1);
        assert!(!entries[1].is_dex);
    }

    // --- New tests for skipped-budget-neutral semantics ---

    #[test]
    fn test_broadcast_visitor_skipped_does_not_consume_generic_budget() {
        let config = TxQueueConfig {
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx_high = make_test_envelope(300, 2);
        set_source(&mut tx_high, 1);
        let mut tx_low = make_test_envelope(100, 1);
        set_source(&mut tx_low, 2);

        queue.try_add(tx_high.clone());
        queue.try_add(tx_low.clone());

        let hash_high = Hash256::hash_xdr(&tx_high);
        let mut budget = BroadcastBudget {
            ops_remaining: 3,
            dex_ops_remaining: None,
        };
        let mut visited = Vec::new();
        queue.broadcast_with_visitor(&mut budget, |candidate| {
            visited.push(candidate.hash);
            if candidate.hash == hash_high {
                BroadcastVisitResult::Skipped
            } else {
                BroadcastVisitResult::Processed
            }
        });
        // Both candidates visited; high was skipped, low was processed
        assert_eq!(visited.len(), 2);
        // Budget: started at 3, only low (1 op) consumed
        assert_eq!(budget.ops_remaining, 2);
    }

    #[test]
    fn test_broadcast_visitor_skipped_does_not_consume_dex_budget() {
        let config = TxQueueConfig {
            max_size: 10,
            max_dex_ops: Some(1000),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex = make_dex_envelope(300, 1);
        set_source(&mut dex, 1);
        let mut non_dex = make_test_envelope(100, 1);
        set_source(&mut non_dex, 2);

        queue.try_add(dex.clone());
        queue.try_add(non_dex.clone());

        let dex_hash = Hash256::hash_xdr(&dex);
        let mut budget = BroadcastBudget {
            ops_remaining: 10,
            dex_ops_remaining: Some(5),
        };
        queue.broadcast_with_visitor(&mut budget, |candidate| {
            if candidate.hash == dex_hash {
                BroadcastVisitResult::Skipped
            } else {
                BroadcastVisitResult::Processed
            }
        });
        // DEX tx skipped → DEX budget unchanged, generic budget consumed only for non-DEX
        assert_eq!(budget.ops_remaining, 9);
        assert_eq!(budget.dex_ops_remaining, Some(5));
    }

    #[test]
    fn test_broadcast_visitor_all_skipped_scans_full_queue() {
        let config = TxQueueConfig {
            max_size: 100,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Add 20 txs
        for i in 1..=20u8 {
            let mut tx = make_test_envelope(100 * i as u32, 1);
            set_source(&mut tx, i);
            queue.try_add(tx);
        }

        let mut visit_count = 0;
        let mut budget = BroadcastBudget {
            ops_remaining: 100,
            dex_ops_remaining: None,
        };
        queue.broadcast_with_visitor(&mut budget, |_| {
            visit_count += 1;
            BroadcastVisitResult::Skipped
        });
        // All 20 candidates visited, none consumed budget
        assert_eq!(visit_count, 20);
        assert_eq!(budget.ops_remaining, 100);
    }

    #[test]
    fn test_broadcast_visitor_budget_carry_over_mixed() {
        let config = TxQueueConfig {
            max_size: 10,
            max_dex_ops: Some(1000),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // 3 DEX txs: 2 ops each
        let mut dex1 = make_dex_envelope(900, 2);
        set_source(&mut dex1, 1);
        let mut dex2 = make_dex_envelope(600, 2);
        set_source(&mut dex2, 2);
        let mut non_dex = make_test_envelope(300, 1);
        set_source(&mut non_dex, 3);

        queue.try_add(dex1.clone());
        queue.try_add(dex2.clone());
        queue.try_add(non_dex.clone());

        let dex1_hash = Hash256::hash_xdr(&dex1);
        let mut budget = BroadcastBudget {
            ops_remaining: 10,
            dex_ops_remaining: Some(5),
        };
        queue.broadcast_with_visitor(&mut budget, |candidate| {
            if candidate.hash == dex1_hash {
                // Skip the first DEX tx
                BroadcastVisitResult::Skipped
            } else {
                BroadcastVisitResult::Processed
            }
        });
        // dex1 (2 ops) skipped → no consumption
        // dex2 (2 ops) processed → ops: 10-2=8, dex: 5-2=3
        // non_dex (1 op) processed → ops: 8-1=7
        assert_eq!(budget.ops_remaining, 7);
        assert_eq!(budget.dex_ops_remaining, Some(3));
    }

    #[test]
    fn test_broadcast_visitor_dex_budget_zero_deactivates_lane() {
        let config = TxQueueConfig {
            max_size: 10,
            max_dex_ops: Some(1000),
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex = make_dex_envelope(300, 1);
        set_source(&mut dex, 1);
        let mut non_dex = make_test_envelope(100, 1);
        set_source(&mut non_dex, 2);

        queue.try_add(dex);
        queue.try_add(non_dex.clone());

        // dex_ops_budget = Some(0): first DEX tx (1 op > 0) deactivates lane
        let (entries, budget) = visit_all_processed(&queue, 100, Some(0));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash, Hash256::hash_xdr(&non_dex));
        assert!(!entries[0].is_dex);
        assert_eq!(budget.dex_ops_remaining, Some(0));
    }

    #[test]
    fn test_broadcast_visitor_exact_fit_boundary() {
        let config = TxQueueConfig {
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut tx = make_test_envelope(200, 2);
        set_source(&mut tx, 1);
        queue.try_add(tx.clone());

        // Budget exactly matches: ops == remaining → should fit (not exceed)
        let (entries, budget) = visit_all_processed(&queue, 2, None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash, Hash256::hash_xdr(&tx));
        assert_eq!(budget.ops_remaining, 0);
    }

    #[test]
    fn test_broadcast_visitor_dex_exact_fit_boundary() {
        let config = TxQueueConfig {
            max_size: 10,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        let mut dex = make_dex_envelope(200, 2);
        set_source(&mut dex, 1);
        queue.try_add(dex.clone());

        // Both generic and DEX budget exactly match
        let (entries, budget) = visit_all_processed(&queue, 2, Some(2));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].hash, Hash256::hash_xdr(&dex));
        assert_eq!(budget.ops_remaining, 0);
        assert_eq!(budget.dex_ops_remaining, Some(0));
    }

    /// Create a path-payment-strict-receive envelope where send_asset → path → dest_asset
    /// forms a loop (send_asset == dest_asset with no path, or forms a cycle).
    fn make_arb_loop_envelope(fee: u32) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let asset_a = Asset::Native;
        let asset_b = Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(*b"USD\0"),
            issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
        });
        // A → B → A forms a loop
        let op = Operation {
            source_account: None,
            body: OperationBody::PathPaymentStrictReceive(PathPaymentStrictReceiveOp {
                send_asset: asset_a.clone(),
                send_max: 1000,
                destination: MuxedAccount::Ed25519(Uint256([2u8; 32])),
                dest_asset: asset_a,
                dest_amount: 100,
                path: vec![asset_b].try_into().unwrap(),
            }),
        };
        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        })
    }

    #[test]
    fn test_broadcast_visitor_arb_damping_drops_after_allowance() {
        let config = TxQueueConfig {
            max_size: 100,
            flood_arb_tx_base_allowance: 1,
            flood_arb_tx_damping_factor: 0.8,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Add several arb-loop txs from different sources
        for i in 0..5u8 {
            let mut env = make_arb_loop_envelope(200);
            set_source(&mut env, 10 + i);
            queue.try_add(env);
        }

        // With base_allowance=1, the first should always be broadcast,
        // subsequent ones may be dampened.
        let (entries, _) = visit_all_processed(&queue, 100, None);
        // At least 1 must pass (the base allowance), but fewer than all 5
        // should pass (probabilistic damping kicks in).
        assert!(
            !entries.is_empty(),
            "At least the base allowance txs should pass"
        );

        // Metrics should reflect arb processing
        let stats = queue.stats();
        assert!(stats.arb_tx_seen > 0, "arb_tx_seen should be incremented");
    }

    #[test]
    fn test_broadcast_visitor_arb_damping_disabled() {
        let config = TxQueueConfig {
            max_size: 100,
            flood_arb_tx_base_allowance: -1, // disabled
            flood_arb_tx_damping_factor: 0.8,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        for i in 0..3u8 {
            let mut env = make_arb_loop_envelope(200);
            set_source(&mut env, 10 + i);
            queue.try_add(env);
        }

        let (entries, _) = visit_all_processed(&queue, 100, None);
        // All should pass when damping is disabled
        assert_eq!(entries.len(), 3);

        // arb_tx_seen/dropped should be 0 when disabled
        let stats = queue.stats();
        assert_eq!(stats.arb_tx_seen, 0);
        assert_eq!(stats.arb_tx_dropped, 0);
    }

    #[test]
    fn test_shift_clears_arb_damper() {
        let config = TxQueueConfig {
            max_size: 100,
            flood_arb_tx_base_allowance: 1,
            flood_arb_tx_damping_factor: 0.8,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Add and broadcast arb txs to populate damper state
        for i in 0..3u8 {
            let mut env = make_arb_loop_envelope(200);
            set_source(&mut env, 10 + i);
            queue.try_add(env);
        }
        visit_all_processed(&queue, 100, None);

        let stats_before = queue.stats();
        assert!(stats_before.arb_tx_seen > 0);

        // shift() should clear the damper's internal state (per-pair counters)
        // but metrics are cumulative AtomicU64s, so they persist.
        queue.shift();
        let damper = queue.arb_damper.lock();
        assert!(
            damper.damping_map.is_empty(),
            "shift() should clear damper's per-pair counters"
        );
    }

    #[test]
    fn test_reset_and_rebuild_preserves_arb_damper() {
        let config = TxQueueConfig {
            max_size: 100,
            flood_arb_tx_base_allowance: 1,
            flood_arb_tx_damping_factor: 0.8,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Add and broadcast arb txs to populate damper state
        for i in 0..3u8 {
            let mut env = make_arb_loop_envelope(200);
            set_source(&mut env, 10 + i);
            queue.try_add(env);
        }
        visit_all_processed(&queue, 100, None);

        // Grab damper state before reset
        let pairs_before = queue.arb_damper.lock().damping_map.len();
        assert!(
            pairs_before > 0,
            "damper should have entries after broadcast"
        );

        // reset_and_rebuild should NOT clear the damper
        queue.reset_and_rebuild();
        let pairs_after = queue.arb_damper.lock().damping_map.len();
        assert_eq!(
            pairs_before, pairs_after,
            "reset_and_rebuild must preserve arb damper state"
        );
    }

    #[test]
    fn test_broadcast_dampened_arb_txs_are_banned() {
        // With base_allowance=1, damping_factor=1.0:
        //   - TX1: allowed (under base allowance, counter 0 → 1)
        //   - TX2: allowed (k=0, geometric sample always >= 0, counter 1 → 2)
        //   - TX3+: dampened (k≥1, geometric sample=0 < k with factor=1.0)
        //
        // Using damping_factor=1.0 makes the geometric distribution always
        // return 0, ensuring deterministic damping for k≥1.
        let config = TxQueueConfig {
            max_size: 100,
            flood_arb_tx_base_allowance: 1,
            flood_arb_tx_damping_factor: 1.0,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Add 5 arb-loop txs from different sources (same asset pair).
        let mut hashes = Vec::new();
        for i in 0..5u8 {
            let mut env = make_arb_loop_envelope(200);
            set_source(&mut env, 10 + i);
            let hash = Hash256::hash_xdr(&env);
            queue.try_add(env);
            hashes.push(hash);
        }

        assert_eq!(queue.len(), 5);
        assert_eq!(queue.banned_count(), 0);

        // Broadcast with large budget — TX1 and TX2 allowed, TX3-5 dampened.
        let mut budget = BroadcastBudget {
            ops_remaining: 100,
            dex_ops_remaining: None,
        };
        let mut visited = Vec::new();
        queue.broadcast_with_visitor(&mut budget, |candidate| {
            visited.push(candidate.hash);
            BroadcastVisitResult::Processed
        });

        // Exactly 2 txs should have been visited (allowed through damping).
        assert_eq!(visited.len(), 2, "Only 2 txs should pass damping");

        // The 3 dampened txs should now be banned and removed from the queue.
        assert_eq!(
            queue.len(),
            2,
            "Queue should only contain 2 non-dampened txs"
        );
        assert_eq!(queue.banned_count(), 3, "3 dampened txs should be banned");

        // Verify specific dampened hashes are banned and not in queue.
        // The dampened ones are those NOT in `visited`.
        let dampened_hashes: Vec<_> = hashes.iter().filter(|h| !visited.contains(h)).collect();
        assert_eq!(dampened_hashes.len(), 3);
        for hash in &dampened_hashes {
            assert!(queue.is_banned(hash), "Dampened tx should be banned");
            assert!(
                !queue.contains(hash),
                "Dampened tx should be removed from queue"
            );
        }

        // A second broadcast should not encounter the dampened hashes at all.
        let mut budget2 = BroadcastBudget {
            ops_remaining: 100,
            dex_ops_remaining: None,
        };
        let mut visited2 = Vec::new();
        queue.broadcast_with_visitor(&mut budget2, |candidate| {
            visited2.push(candidate.hash);
            BroadcastVisitResult::Processed
        });

        // The remaining 2 txs may or may not pass damping again (their counter
        // is now 2, so k=1, dampened with factor=1.0). But crucially, the 3
        // banned txs are never seen.
        for hash in &dampened_hashes {
            assert!(
                !visited2.contains(hash),
                "Banned tx must not be revisited on subsequent broadcast"
            );
        }
    }

    #[test]
    fn test_stats_pending_txs_age_histogram() {
        // Stagger 3 transactions at different ages and verify the age histogram.
        let queue = TransactionQueue::with_depths(TxQueueConfig::default(), 10, 4);

        // Add TX A at age 0
        let mut tx_a = make_test_envelope(200, 1);
        set_source(&mut tx_a, 80);
        assert_eq!(queue.try_add(tx_a), TxQueueResult::Added);

        // Shift → TX A age becomes 1
        queue.shift();

        // Add TX B at age 0
        let mut tx_b = make_test_envelope(200, 1);
        set_source(&mut tx_b, 81);
        assert_eq!(queue.try_add(tx_b), TxQueueResult::Added);

        // Shift → TX A age=2, TX B age=1
        queue.shift();

        // Add TX C at age 0
        let mut tx_c = make_test_envelope(200, 1);
        set_source(&mut tx_c, 82);
        assert_eq!(queue.try_add(tx_c), TxQueueResult::Added);

        // Now: TX A age=2, TX B age=1, TX C age=0
        let stats = queue.stats();
        assert_eq!(stats.pending_count, 3);
        assert_eq!(stats.pending_txs_age, [1, 1, 1, 0]);
    }

    #[test]
    fn test_stats_pending_txs_age_excludes_fee_source_only() {
        // A fee-bump TX creates an account_state entry for the fee-source
        // (when distinct from seq-source) with transaction=None.
        // That entry should NOT inflate pending_txs_age[0].
        let queue = TransactionQueue::with_depths(TxQueueConfig::default(), 10, 4);

        // Build a fee-bump: inner TX has seq-source seed=90, outer fee-source seed=91.
        let mut inner = make_test_envelope(200, 1);
        set_source(&mut inner, 90);
        let inner_v1 = match inner {
            TransactionEnvelope::Tx(env) => env,
            _ => panic!("Expected Tx variant"),
        };
        let fee_source = MuxedAccount::Ed25519(Uint256([91u8; 32]));
        let fee_bump = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: FeeBumpTransaction {
                fee_source,
                fee: 400,
                inner_tx: FeeBumpTransactionInnerTx::Tx(inner_v1),
                ext: FeeBumpTransactionExt::V0,
            },
            signatures: vec![DecoratedSignature {
                hint: SignatureHint([0u8; 4]),
                signature: Signature(vec![0u8; 64].try_into().unwrap()),
            }]
            .try_into()
            .unwrap(),
        });

        assert_eq!(queue.try_add(fee_bump), TxQueueResult::Added);

        let stats = queue.stats();
        // Only 1 pending tx (on seq-source account 90), age bucket [0] = 1.
        // The fee-source-only entry (account 91) should NOT be counted.
        assert_eq!(stats.pending_count, 1);
        assert_eq!(stats.pending_txs_age, [1, 0, 0, 0]);
        assert_eq!(stats.account_count, 1);
    }

    #[test]
    fn test_stats_pending_txs_age_clamp_at_3() {
        // With pending_depth=6, a tx can reach age 5 before eviction.
        // The age histogram should clamp it into bucket [3].
        let queue = TransactionQueue::with_depths(TxQueueConfig::default(), 10, 6);

        let mut tx = make_test_envelope(200, 1);
        set_source(&mut tx, 91);
        assert_eq!(queue.try_add(tx), TxQueueResult::Added);

        // Shift 5 times → age = 5 (still < pending_depth=6, not evicted)
        for _ in 0..5 {
            queue.shift();
        }

        let stats = queue.stats();
        assert_eq!(stats.pending_count, 1);
        // Age 5 should clamp into bucket [3]
        assert_eq!(stats.pending_txs_age, [0, 0, 0, 1]);
    }

    /// #3845: `estimate_heap_bytes` is ~0 on an empty queue and grows once
    /// transactions (and bans) populate the owned collections.
    #[test]
    fn test_tx_queue_estimate_heap_bytes_grows() {
        let queue = TransactionQueue::with_ban_depth(TxQueueConfig::default(), 3);
        let empty = queue.estimate_heap_bytes();

        let tx1 = make_test_envelope(200, 1);
        assert_eq!(queue.try_add(tx1), TxQueueResult::Added);
        let with_one = queue.estimate_heap_bytes();
        assert!(
            with_one > empty,
            "adding a transaction must increase the heap estimate ({with_one} !> {empty})"
        );

        // Banning populates the banned-transactions deque.
        let mut tx2 = make_test_envelope(200, 1);
        set_source(&mut tx2, 2);
        queue.ban(&[Hash256::hash_xdr(&tx2)]);
        assert!(queue.estimate_heap_bytes() >= with_one);
    }
}
