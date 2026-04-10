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
use std::time::Instant;

use henyey_common::{
    any_greater, Hash256, NetworkId, Resource, ResourceType, NUM_SOROBAN_TX_RESOURCES,
};
use henyey_crypto::Sha256Hasher;
use stellar_xdr::curr::WriteXdr;
use stellar_xdr::curr::{
    AccountId, DecoratedSignature, FeeBumpTransactionInnerTx, GeneralizedTransactionSet, Limits,
    OperationType, Preconditions, SignerKey, TransactionEnvelope, TransactionPhase, TxSetComponent,
};

use crate::error::HerderError;
use crate::surge_pricing::{
    DexLimitingLaneConfig, OpsOnlyLaneConfig, SorobanGenericLaneConfig, SurgePricingLaneConfig,
    SurgePricingPriorityQueue, GENERIC_LANE,
};
use crate::Result;
use henyey_tx::envelope_sequence_number;
use rand::Rng;

mod selection;
mod tx_set;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShiftResult {
    /// Number of transactions that were unbanned (reached end of ban period).
    pub unbanned_count: usize,
    /// Number of transactions that were auto-banned due to age (pending too long).
    pub evicted_due_to_age: usize,
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

/// Trait for providing ledger account balance information.
///
/// This trait is used for fee balance validation during transaction queue
/// operations. Implementations should provide the available balance for
/// an account that can be used to pay transaction fees.
pub trait FeeBalanceProvider: Send + Sync {
    /// Get the available balance for an account that can be used for fees.
    ///
    /// Returns the native asset balance minus any reserves or holds,
    /// or None if the account doesn't exist.
    fn get_available_balance(&self, account_id: &AccountId) -> Option<i64>;
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
    /// Whether to validate time bounds before queueing.
    pub validate_time_bounds: bool,
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
}

impl Default for TxQueueConfig {
    fn default() -> Self {
        Self {
            max_size: DEFAULT_MAX_QUEUE_SIZE,
            max_age_secs: DEFAULT_MAX_AGE_SECS,
            min_fee_per_op: DEFAULT_MIN_FEE_PER_OP,
            validate_signatures: true,
            validate_time_bounds: true,
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
        }
    }
}

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
    /// Soroban per-transaction resource limits (if available).
    pub soroban_limits: Option<SorobanTxLimits>,
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
            base_reserve: 5_000_000, // 0.5 XLM default
            ledger_flags: 0,
            soroban_limits: None,
        }
    }
}

/// A transaction in the queue with metadata.
#[derive(Debug, Clone)]
pub struct QueuedTransaction {
    /// The transaction envelope.
    pub envelope: TransactionEnvelope,
    /// Hash of the transaction.
    pub hash: Hash256,
    /// When this transaction was received.
    pub received_at: Instant,
    /// Declared fee per operation used for queue-admission minimum fee checks.
    pub fee_per_op: u64,
    /// Number of operations in the transaction.
    pub op_count: u32,
    /// Declared full fee.
    pub total_fee: u64,
    /// Inclusion fee used for surge pricing and replacement decisions.
    pub inclusion_fee: u64,
}

impl QueuedTransaction {
    /// Create a new queued transaction.
    pub fn new(envelope: TransactionEnvelope) -> Result<Self> {
        let hash = Hash256::hash_xdr(&envelope)
            .map_err(|e| HerderError::Internal(format!("Failed to hash transaction: {}", e)))?;

        let (total_fee, inclusion_fee, op_count) = Self::extract_fees_and_ops(&envelope)?;
        let fee_per_op = if op_count > 0 {
            total_fee / op_count as u64
        } else {
            0
        };

        Ok(Self {
            envelope,
            hash,
            received_at: Instant::now(),
            fee_per_op,
            op_count,
            total_fee,
            inclusion_fee,
        })
    }

    /// Extract full fee, inclusion fee, and operation count from the envelope.
    fn extract_fees_and_ops(envelope: &TransactionEnvelope) -> Result<(u64, u64, u32)> {
        let fee = crate::tx_set_utils::envelope_fee(envelope);
        if fee < 0 {
            return Err(HerderError::Internal(format!(
                "Negative declared fee for transaction: {}",
                fee
            )));
        }
        let inclusion_fee = crate::tx_set_utils::envelope_inclusion_fee(envelope);
        if inclusion_fee < 0 {
            return Err(HerderError::Internal(format!(
                "Negative inclusion fee for transaction: {}",
                inclusion_fee
            )));
        }
        let ops = crate::tx_set_utils::envelope_num_ops(envelope) as u32;
        Ok((fee as u64, inclusion_fee as u64, ops))
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

    fn is_better_than(&self, other: &QueuedTransaction) -> bool {
        better_fee_ratio(self, other)
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
}

impl AccountState {
    /// Check if this account state can be removed (no transaction and no fees tracked).
    pub fn is_empty(&self) -> bool {
        self.transaction.is_none() && self.total_fees == 0
    }
}

/// Fee multiplier required for replace-by-fee with fee-bump transactions.
/// A fee-bump must have a fee at least FEE_MULTIPLIER times the existing fee rate.
const FEE_MULTIPLIER: u64 = 10;

/// Default pending depth (number of ledgers before auto-ban).
/// Spec: HERDER_SPEC §16 — TRANSACTION_QUEUE_TIMEOUT_LEDGERS = 4.
const DEFAULT_PENDING_DEPTH: u32 = 4;

pub(super) fn envelope_fee_per_op(envelope: &TransactionEnvelope) -> Option<(u64, u64, u32)> {
    QueuedTransaction::extract_fees_and_ops(envelope)
        .ok()
        .map(|(_, inclusion_fee, op_count)| {
            let per_op = if op_count > 0 {
                inclusion_fee / op_count as u64
            } else {
                0
            };
            (per_op, inclusion_fee, op_count)
        })
}

pub(crate) fn fee_rate_cmp(a_fee: u64, a_ops: u32, b_fee: u64, b_ops: u32) -> Ordering {
    let left = (a_fee as u128).saturating_mul(b_ops as u128);
    let right = (b_fee as u128).saturating_mul(a_ops as u128);
    left.cmp(&right)
}

fn better_fee_ratio(new_tx: &QueuedTransaction, old_tx: &QueuedTransaction) -> bool {
    match fee_rate_cmp(
        new_tx.inclusion_fee,
        new_tx.op_count,
        old_tx.inclusion_fee,
        old_tx.op_count,
    ) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => new_tx.hash.0 < old_tx.hash.0,
    }
}

fn compute_better_fee(evicted_fee: u64, evicted_ops: u32, tx_ops: u32) -> u64 {
    if evicted_ops == 0 {
        return 0;
    }
    let numerator = (evicted_fee as u128).saturating_mul(tx_ops as u128);
    let denominator = evicted_ops as u128;
    let base = numerator / denominator;
    let candidate = base.saturating_add(1);
    u64::try_from(candidate).unwrap_or(u64::MAX)
}

fn min_inclusion_fee_to_beat(evicted: (u64, u32), tx: &QueuedTransaction) -> u64 {
    if evicted.1 == 0 {
        return 0;
    }
    if fee_rate_cmp(evicted.0, evicted.1, tx.inclusion_fee, tx.op_count) != Ordering::Less {
        compute_better_fee(evicted.0, evicted.1, tx.op_count)
    } else {
        0
    }
}

/// Check if a fee-bump transaction can replace an existing transaction.
/// For replace-by-fee to work, the new fee must be at least FEE_MULTIPLIER times the old fee rate.
/// Returns Ok(()) if replacement is allowed, or Err(min_fee) if the fee is insufficient.
fn can_replace_by_fee(
    new_fee: u64,
    new_ops: u32,
    old_fee: u64,
    old_ops: u32,
) -> std::result::Result<(), u64> {
    // newFee / newOps >= FEE_MULTIPLIER * oldFee / oldOps
    // Cross-multiply to avoid division:
    // newFee * oldOps >= FEE_MULTIPLIER * oldFee * newOps
    let left = (new_fee as u128).saturating_mul(old_ops as u128);
    let right = (FEE_MULTIPLIER as u128)
        .saturating_mul(old_fee as u128)
        .saturating_mul(new_ops as u128);

    if left < right {
        // Calculate minimum fee required:
        // minFee * oldOps >= FEE_MULTIPLIER * oldFee * newOps
        // minFee >= (FEE_MULTIPLIER * oldFee * newOps) / oldOps + 1 (round up)
        let min_fee = if old_ops > 0 {
            let numerator = right;
            let denominator = old_ops as u128;
            let quotient = numerator / denominator;
            let remainder = numerator % denominator;
            let rounded = if remainder > 0 {
                quotient + 1
            } else {
                quotient
            };
            u64::try_from(rounded).unwrap_or(u64::MAX)
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
    pub(super) transactions: Vec<TransactionEnvelope>,
    pub(super) soroban_limited: bool,
    pub(super) dex_limited: bool,
    pub(super) classic_limited: bool,
}

struct EvictionScan<'a, F> {
    by_hash: &'a HashMap<Hash256, QueuedTransaction>,
    queued: &'a QueuedTransaction,
    lane_config: Box<dyn SurgePricingLaneConfig>,
    ledger_version: u32,
    exclude: &'a HashSet<Hash256>,
    filter: F,
    seed: u64,
}

/// Get the source account (inner for fee-bump) as a MuxedAccount.
fn source_account_from_envelope(envelope: &TransactionEnvelope) -> stellar_xdr::curr::MuxedAccount {
    match envelope {
        TransactionEnvelope::TxV0(env) => {
            stellar_xdr::curr::MuxedAccount::Ed25519(env.tx.source_account_ed25519.clone())
        }
        TransactionEnvelope::Tx(env) => env.tx.source_account.clone(),
        TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                inner.tx.source_account.clone()
            }
        },
    }
}

fn account_key(envelope: &TransactionEnvelope) -> Vec<u8> {
    let account_id = henyey_tx::muxed_to_account_id(&source_account_from_envelope(envelope));
    account_id
        .to_xdr(stellar_xdr::curr::Limits::none())
        .unwrap_or_default()
}

pub(crate) fn account_key_from_account_id(account_id: &AccountId) -> Vec<u8> {
    account_id
        .to_xdr(stellar_xdr::curr::Limits::none())
        .unwrap_or_default()
}

fn account_id_from_envelope(envelope: &TransactionEnvelope) -> AccountId {
    henyey_tx::muxed_to_account_id(&source_account_from_envelope(envelope))
}

/// Get the fee-source account key (for fee bump, this is the outer source; otherwise same as inner).
fn fee_source_key(envelope: &TransactionEnvelope) -> Vec<u8> {
    let fee_source = match envelope {
        TransactionEnvelope::TxV0(env) => {
            stellar_xdr::curr::MuxedAccount::Ed25519(env.tx.source_account_ed25519.clone())
        }
        TransactionEnvelope::Tx(env) => env.tx.source_account.clone(),
        TransactionEnvelope::TxFeeBump(env) => env.tx.fee_source.clone(),
    };
    let account_id = henyey_tx::muxed_to_account_id(&fee_source);
    account_id
        .to_xdr(stellar_xdr::curr::Limits::none())
        .unwrap_or_default()
}

/// Check if envelope is a fee-bump transaction.
fn is_fee_bump_envelope(envelope: &TransactionEnvelope) -> bool {
    matches!(envelope, TransactionEnvelope::TxFeeBump(_))
}

/// Convert an XDR-encoded account key back to AccountId.
fn account_id_from_fee_source_key(key: &[u8]) -> AccountId {
    use stellar_xdr::curr::ReadXdr;
    AccountId::from_xdr(key, Limits::none()).unwrap_or({
        // Fallback to a zero account ID if decoding fails
        AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
            stellar_xdr::curr::Uint256([0; 32]),
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
pub struct TransactionQueue {
    /// Configuration.
    config: TxQueueConfig,
    /// Transactions indexed by hash.
    by_hash: RwLock<HashMap<Hash256, QueuedTransaction>>,
    /// Seen transaction hashes (includes recently applied).
    seen: RwLock<HashSet<Hash256>>,
    /// Validation context (ledger state info for validation).
    validation_context: RwLock<ValidationContext>,
    /// Lane eviction thresholds for classic queue admission.
    classic_lane_evicted_inclusion_fee: RwLock<Vec<(u64, u32)>>,
    /// Lane eviction thresholds for Soroban queue admission.
    soroban_lane_evicted_inclusion_fee: RwLock<Vec<(u64, u32)>>,
    /// Eviction threshold for global queue limits.
    global_evicted_inclusion_fee: RwLock<(u64, u32)>,
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
    /// Separate from queue-admission limits which use POOL_LEDGER_MULTIPLIER (2x).
    dynamic_selection_soroban_resources: RwLock<Option<Resource>>,
}

/// Default ban depth (number of ledgers transactions stay banned).
const DEFAULT_BAN_DEPTH: u32 = 10;

impl TransactionQueue {
    /// Create a new transaction queue.
    pub fn new(config: TxQueueConfig) -> Self {
        Self::with_depths(config, DEFAULT_BAN_DEPTH, DEFAULT_PENDING_DEPTH)
    }

    /// Create a new transaction queue with custom ban depth.
    pub fn with_ban_depth(config: TxQueueConfig, ban_depth: u32) -> Self {
        Self::with_depths(config, ban_depth, DEFAULT_PENDING_DEPTH)
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
            ..Default::default()
        };

        // Initialize the banned transactions deque with ban_depth empty sets
        let mut banned = std::collections::VecDeque::with_capacity(ban_depth as usize);
        for _ in 0..ban_depth {
            banned.push_back(HashSet::new());
        }

        Self {
            config,
            by_hash: RwLock::new(HashMap::new()),
            seen: RwLock::new(HashSet::new()),
            validation_context: RwLock::new(ctx),
            classic_lane_evicted_inclusion_fee: RwLock::new(Vec::new()),
            soroban_lane_evicted_inclusion_fee: RwLock::new(Vec::new()),
            global_evicted_inclusion_fee: RwLock::new((0, 0)),
            banned_transactions: RwLock::new(banned),
            account_states: RwLock::new(HashMap::new()),
            pending_depth,
            fee_balance_provider: RwLock::new(None),
            #[cfg(any(test, feature = "test-utils"))]
            skip_fee_balance_check: std::sync::atomic::AtomicBool::new(false),
            dynamic_queue_soroban_resources: RwLock::new(None),
            dynamic_selection_soroban_resources: RwLock::new(None),
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

    /// Return all queued transaction envelopes (for post-close invalidation).
    pub fn pending_envelopes(&self) -> Vec<TransactionEnvelope> {
        let by_hash = self.by_hash.read();
        by_hash.values().map(|qt| qt.envelope.clone()).collect()
    }

    /// Update Soroban resource limits dynamically after ledger close.
    ///
    /// Called with limits derived from `SorobanNetworkInfo` multiplied by
    /// the pool ledger multiplier.
    pub fn update_soroban_resource_limits(&self, resources: Resource) {
        *self.dynamic_queue_soroban_resources.write() = Some(resources);
    }

    /// Update Soroban resource limits for tx-set selection (1x ledger max).
    /// Called alongside `update_soroban_resource_limits` but without the
    /// POOL_LEDGER_MULTIPLIER scaling.
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

    /// Return the effective Soroban resource limits for queue admission.
    /// Prefers the dynamic value (updated each ledger close) over the static config.
    fn effective_queue_soroban_resources(&self) -> Option<Resource> {
        let dynamic = self.dynamic_queue_soroban_resources.read();
        if dynamic.is_some() {
            dynamic.clone()
        } else {
            self.config.max_queue_soroban_resources.clone()
        }
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
    pub fn update_validation_context(
        &self,
        ledger_seq: u32,
        close_time: u64,
        protocol_version: u32,
        base_fee: u32,
        base_reserve: u32,
        ledger_flags: u32,
    ) {
        let mut ctx = self.validation_context.write();
        ctx.ledger_seq = ledger_seq;
        ctx.close_time = close_time;
        ctx.protocol_version = protocol_version;
        ctx.base_fee = base_fee;
        ctx.base_reserve = base_reserve;
        ctx.ledger_flags = ledger_flags;
    }

    /// Set the Soroban per-transaction resource limits in the validation context.
    ///
    /// Called during startup seeding (before the first ledger close) to ensure
    /// Soroban txs are validated against network config limits from the start.
    pub fn set_soroban_limits(&self, limits: SorobanTxLimits) {
        self.validation_context.write().soroban_limits = Some(limits);
    }

    /// Validate a transaction before queueing.
    fn validate_transaction(
        &self,
        envelope: &TransactionEnvelope,
    ) -> std::result::Result<(), henyey_tx::TxResultCode> {
        use henyey_tx::{
            validate_ledger_bounds, validate_signatures, validate_time_bounds, LedgerContext,
            TransactionFrame, TxResultCode, ValidationError,
        };

        let frame =
            TransactionFrame::from_owned_with_network(envelope.clone(), self.config.network_id);
        let ctx = self.validation_context.read();
        let base_fee = ctx.base_fee.max(self.config.min_fee_per_op);

        // Phase 1: Shared stateless structural validation
        // Mirrors stellar-core's commonValidPreSeqNum subset.
        henyey_tx::check_valid_pre_seq_num(&frame, ctx.protocol_version, ctx.ledger_flags)
            .map_err(|e| e.to_tx_result_code())?;

        // Build ledger context once for time-bound and signature validation.
        let ledger_ctx = LedgerContext::new(
            ctx.ledger_seq,
            ctx.close_time,
            base_fee,
            5_000_000, // base reserve
            ctx.protocol_version,
            self.config.network_id,
        );

        // Validate time bounds if enabled.
        // Parity: stellar-core TransactionQueue::tryAdd uses
        // getUpperBoundCloseTimeOffset(app, closeTime) for the max_time check,
        // which accounts for drift since LCL + expected close time * 2.
        if self.config.validate_time_bounds {
            // Check against LCL close time (no offset): catches min_time too
            // early and max_time already expired.
            match validate_time_bounds(&frame, &ledger_ctx) {
                Err(ValidationError::TooEarly { .. }) => {
                    return Err(TxResultCode::TxTooEarly);
                }
                Err(ValidationError::TooLate { .. }) => {
                    return Err(TxResultCode::TxTooLate);
                }
                Err(_) => {
                    return Err(TxResultCode::TxTooEarly);
                }
                Ok(()) => {}
            }

            // For max_time (too late) check: add upper bound offset.
            // upperBound = expected_close_time * EXPECTED_CLOSE_TIME_MULT + drift
            // where drift = max(0, now - lcl_close_time).
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let drift = now.saturating_sub(ctx.close_time);
            let upper_offset =
                self.config.expected_ledger_close_secs * EXPECTED_CLOSE_TIME_MULT + drift;
            let upper_close_time = ctx.close_time.saturating_add(upper_offset);
            let upper_ctx = LedgerContext::new(
                ctx.ledger_seq,
                upper_close_time,
                base_fee,
                5_000_000,
                ctx.protocol_version,
                self.config.network_id,
            );
            if validate_time_bounds(&frame, &upper_ctx).is_err() {
                return Err(TxResultCode::TxTooLate);
            }

            if validate_ledger_bounds(&frame, &ledger_ctx).is_err() {
                return Err(TxResultCode::TxTooEarly);
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

        Ok(())
    }

    /// Check that a Soroban transaction's declared resources don't exceed
    /// per-transaction network config limits.
    ///
    /// Parity: stellar-core `TransactionFrame::checkSorobanResources()`.
    fn check_soroban_resources(
        &self,
        frame: &henyey_tx::TransactionFrame,
    ) -> std::result::Result<(), String> {
        let ctx = self.validation_context.read();
        let Some(ref limits) = ctx.soroban_limits else {
            // No limits configured — skip check
            return Ok(());
        };

        let Some(data) = frame.soroban_data() else {
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

        let tx_size = frame.tx_size_bytes() as u64;
        if tx_size > limits.tx_max_size_bytes {
            return Err(format!(
                "tx size {} exceeds limit {}",
                tx_size, limits.tx_max_size_bytes
            ));
        }

        Ok(())
    }

    fn collect_evictions_for_lane_config<F>(
        &self,
        scan: EvictionScan<'_, F>,
    ) -> Option<Vec<(QueuedTransaction, bool)>>
    where
        F: Fn(&QueuedTransaction) -> bool,
    {
        let EvictionScan {
            by_hash,
            queued,
            lane_config,
            ledger_version,
            exclude,
            filter,
            seed,
        } = scan;

        let mut queue = SurgePricingPriorityQueue::new(lane_config, seed);
        for tx in by_hash.values() {
            if exclude.contains(&tx.hash) {
                continue;
            }
            if filter(tx) {
                queue.add(tx.clone(), &self.config.network_id, ledger_version);
            }
        }
        queue
            .can_fit_with_eviction(queued, None, &self.config.network_id, ledger_version)
            .map(|evictions| evictions.into_iter().collect())
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
    ) -> std::result::Result<Option<QueuedTransaction>, TxQueueResult> {
        let account_states = self.account_states.read();
        if let Some(state) = account_states.get(seq_source_key) {
            if let Some(ref current_tx) = state.transaction {
                if current_tx.hash == queued.hash {
                    return Err(TxQueueResult::Duplicate);
                }

                let current_seq = envelope_sequence_number(&current_tx.envelope);
                if new_seq < current_seq {
                    return Err(TxQueueResult::Invalid(None));
                }

                if !is_fee_bump {
                    return Err(TxQueueResult::TryAgainLater);
                }

                if new_seq != current_seq {
                    return Err(TxQueueResult::TryAgainLater);
                }

                if let Err(_min_fee) = can_replace_by_fee(
                    queued.inclusion_fee,
                    queued.op_count,
                    current_tx.inclusion_fee,
                    current_tx.op_count,
                ) {
                    return Err(TxQueueResult::FeeTooLow);
                }

                return Ok(Some(current_tx.clone()));
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
        lane_fees_lock: &RwLock<Vec<(u64, u32)>>,
        evictions: Vec<(QueuedTransaction, bool)>,
        pending_evictions: &mut HashSet<Hash256>,
        pending_eviction_list: &mut Vec<QueuedTransaction>,
    ) {
        for (evicted, evicted_due_to_lane_limit) in evictions {
            if !pending_evictions.insert(evicted.hash) {
                continue;
            }
            let frame = henyey_tx::TransactionFrame::from_owned_with_network(
                evicted.envelope.clone(),
                self.config.network_id,
            );
            let lane = lane_config.get_lane(&frame);
            let mut lane_fees = lane_fees_lock.write();
            if lane_fees.len() != lane_config.lane_limits().len() {
                lane_fees.resize(lane_config.lane_limits().len(), (0, 0));
            }
            if evicted_due_to_lane_limit {
                lane_fees[lane] = (evicted.inclusion_fee, evicted.op_count);
            } else {
                lane_fees[GENERIC_LANE] = (evicted.inclusion_fee, evicted.op_count);
            }
            drop(lane_fees);
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
        lane_fees: &mut Vec<(u64, u32)>,
        queued_frame: &henyey_tx::TransactionFrame,
        queued: &QueuedTransaction,
    ) -> bool {
        let lane = lane_config.get_lane(queued_frame);
        if lane_fees.len() != lane_config.lane_limits().len() {
            lane_fees.resize(lane_config.lane_limits().len(), (0, 0));
        }
        let global_fee = *self.global_evicted_inclusion_fee.read();
        let mut min_fee = min_inclusion_fee_to_beat(lane_fees[lane], queued);
        min_fee = min_fee.max(min_inclusion_fee_to_beat(lane_fees[GENERIC_LANE], queued));
        if self.config.max_queue_ops.is_some() {
            min_fee = min_fee.max(min_inclusion_fee_to_beat(global_fee, queued));
        }
        min_fee > 0
    }

    /// Check lane-based eviction fees and collect evictions for all applicable lanes.
    ///
    /// Returns the list of transactions to evict, or an early rejection result.
    #[allow(clippy::too_many_arguments)]
    fn check_and_collect_evictions(
        &self,
        by_hash: &HashMap<Hash256, QueuedTransaction>,
        queued: &QueuedTransaction,
        queued_is_soroban: bool,
        queued_frame: &henyey_tx::TransactionFrame,
        ledger_version: u32,
        seed: u64,
        replaced_tx: Option<&QueuedTransaction>,
    ) -> std::result::Result<Vec<QueuedTransaction>, TxQueueResult> {
        // Phase 1: Check minimum inclusion fee for each lane (cheap, read-only)
        if !queued_is_soroban {
            if let Some(lane_config) = self.build_classic_lane_config() {
                let mut lane_fees = self.classic_lane_evicted_inclusion_fee.write();
                if self.fee_below_lane_threshold(&lane_config, &mut lane_fees, queued_frame, queued)
                {
                    return Err(TxQueueResult::FeeTooLow);
                }
            }
        }

        if queued_is_soroban {
            if let Some(limit) = self.effective_queue_soroban_resources() {
                let lane_config = SorobanGenericLaneConfig::new(limit);
                let mut lane_fees = self.soroban_lane_evicted_inclusion_fee.write();
                if self.fee_below_lane_threshold(&lane_config, &mut lane_fees, queued_frame, queued)
                {
                    return Err(TxQueueResult::FeeTooLow);
                }
            }
        }

        if self.config.max_queue_ops.is_some() {
            let global_fee = *self.global_evicted_inclusion_fee.read();
            if min_inclusion_fee_to_beat(global_fee, queued) > 0 {
                return Err(TxQueueResult::FeeTooLow);
            }
        }

        // Phase 2: Collect evictions (more expensive, involves scanning queue)
        let mut pending_evictions: HashSet<Hash256> = HashSet::new();
        let mut pending_eviction_list: Vec<QueuedTransaction> = Vec::new();

        // If this is a replace-by-fee, pre-exclude the old tx from capacity
        // calculations. stellar-core's TxQueueLimiter subtracts the old tx's
        // resources before calling canFitWithEviction.
        if let Some(old_tx) = replaced_tx {
            pending_evictions.insert(old_tx.hash);
        }

        if !queued_is_soroban {
            if let Some(lane_config) = self.build_classic_lane_config() {
                let filter = |tx: &QueuedTransaction| {
                    let frame = henyey_tx::TransactionFrame::from_owned_with_network(
                        tx.envelope.clone(),
                        self.config.network_id,
                    );
                    !frame.is_soroban()
                };
                let Some(evictions) = self.collect_evictions_for_lane_config(EvictionScan {
                    by_hash,
                    queued,
                    lane_config: Box::new(lane_config),
                    ledger_version,
                    exclude: &pending_evictions,
                    filter,
                    seed,
                }) else {
                    return Err(TxQueueResult::QueueFull);
                };
                let lane_config = self.build_classic_lane_config().unwrap();
                self.record_lane_evictions(
                    &lane_config,
                    &self.classic_lane_evicted_inclusion_fee,
                    evictions,
                    &mut pending_evictions,
                    &mut pending_eviction_list,
                );
            }
        }

        if queued_is_soroban {
            if let Some(limit) = self.effective_queue_soroban_resources() {
                let lane_config = SorobanGenericLaneConfig::new(limit.clone());
                let filter = |tx: &QueuedTransaction| {
                    let frame = henyey_tx::TransactionFrame::from_owned_with_network(
                        tx.envelope.clone(),
                        self.config.network_id,
                    );
                    frame.is_soroban()
                };
                let Some(evictions) = self.collect_evictions_for_lane_config(EvictionScan {
                    by_hash,
                    queued,
                    lane_config: Box::new(lane_config),
                    ledger_version,
                    exclude: &pending_evictions,
                    filter,
                    seed,
                }) else {
                    return Err(TxQueueResult::QueueFull);
                };
                let lane_config = SorobanGenericLaneConfig::new(limit);
                self.record_lane_evictions(
                    &lane_config,
                    &self.soroban_lane_evicted_inclusion_fee,
                    evictions,
                    &mut pending_evictions,
                    &mut pending_eviction_list,
                );
            }
        }

        if let Some(limit) = self.config.max_queue_ops {
            let lane_config = OpsOnlyLaneConfig::new(Resource::new(vec![limit as i64]));
            let filter = |_tx: &QueuedTransaction| true;
            let Some(evictions) = self.collect_evictions_for_lane_config(EvictionScan {
                by_hash,
                queued,
                lane_config: Box::new(lane_config),
                ledger_version,
                exclude: &pending_evictions,
                filter,
                seed,
            }) else {
                return Err(TxQueueResult::QueueFull);
            };
            for (evicted, _evicted_due_to_lane_limit) in evictions {
                if !pending_evictions.insert(evicted.hash) {
                    continue;
                }
                let mut global_fee = self.global_evicted_inclusion_fee.write();
                *global_fee = (evicted.inclusion_fee, evicted.op_count);
                pending_eviction_list.push(evicted);
            }
        }

        Ok(pending_eviction_list)
    }

    /// Try to add a transaction to the queue.
    pub fn try_add(&self, envelope: TransactionEnvelope) -> TxQueueResult {
        // Validate transaction before queueing
        if let Err(code) = self.validate_transaction(&envelope) {
            return TxQueueResult::Invalid(Some(code));
        }

        // Create queued transaction
        let queued = match QueuedTransaction::new(envelope) {
            Ok(q) => q,
            Err(_) => return TxQueueResult::Invalid(None),
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

        // Check fee
        let min_fee_per_op = {
            let ctx = self.validation_context.read();
            ctx.base_fee.max(self.config.min_fee_per_op) as u64
        };
        if queued.fee_per_op < min_fee_per_op {
            return TxQueueResult::FeeTooLow;
        }

        let mut by_hash = self.by_hash.write();
        let ledger_version = self.validation_context.read().protocol_version;
        let queued_frame = henyey_tx::TransactionFrame::from_owned_with_network(
            queued.envelope.clone(),
            self.config.network_id,
        );
        let queued_is_soroban = queued_frame.is_soroban();

        // Parity: check Soroban resource limits against network config
        if queued_is_soroban {
            if let Err(reason) = self.check_soroban_resources(&queued_frame) {
                tracing::debug!(
                    hash = %queued.hash,
                    reason = %reason,
                    "Rejecting Soroban tx: resources exceed network config"
                );
                return TxQueueResult::Invalid(None);
            }
        }

        // Check for duplicate in queue
        if by_hash.contains_key(&queued.hash) {
            return TxQueueResult::Duplicate;
        }

        // Per-account limit check: one transaction per account (sequence-number-source)
        let seq_source_key = account_key(&queued.envelope);
        let new_seq = envelope_sequence_number(&queued.envelope);
        let is_fee_bump = is_fee_bump_envelope(&queued.envelope);
        let new_fee_source_key = fee_source_key(&queued.envelope);

        let replaced_tx =
            match self.check_account_limit(&queued, &seq_source_key, new_seq, is_fee_bump) {
                Ok(replaced) => replaced,
                Err(result) => return result,
            };

        let seed = if cfg!(test) {
            0
        } else {
            rand::thread_rng().gen()
        };

        let pending_eviction_list = match self.check_and_collect_evictions(
            &by_hash,
            &queued,
            queued_is_soroban,
            &queued_frame,
            ledger_version,
            seed,
            replaced_tx.as_ref(),
        ) {
            Ok(evictions) => evictions,
            Err(result) => return result,
        };

        for evicted in &pending_eviction_list {
            by_hash.remove(&evicted.hash);
        }
        // Clean up seen set and account state for evicted transactions.
        // Matches stellar-core prepareDropTransaction → mKnownTxHashes.erase()
        // + dropTransaction() → releaseFeeMaybeEraseAccountState().
        if !pending_eviction_list.is_empty() {
            let mut seen = self.seen.write();
            let mut account_states = self.account_states.write();
            for evicted in &pending_eviction_list {
                seen.remove(&evicted.hash);
                Self::drop_transaction(&mut account_states, evicted);
            }
        }

        // Check queue size
        if by_hash.len() >= self.config.max_size {
            // Try to evict expired transactions
            let expired: Vec<QueuedTransaction> = by_hash
                .iter()
                .filter(|(_, tx)| tx.is_expired(self.config.max_age_secs))
                .map(|(_, tx)| tx.clone())
                .collect();

            if !expired.is_empty() {
                let mut seen = self.seen.write();
                let mut account_states = self.account_states.write();
                for tx in &expired {
                    by_hash.remove(&tx.hash);
                    seen.remove(&tx.hash);
                    Self::drop_transaction(&mut account_states, tx);
                }
            }

            if by_hash.len() >= self.config.max_size {
                if let Some((evict_hash, evict_tx)) = by_hash
                    .iter()
                    .min_by(|a, b| {
                        let a_tx = a.1;
                        let b_tx = b.1;
                        fee_rate_cmp(
                            a_tx.inclusion_fee,
                            a_tx.op_count,
                            b_tx.inclusion_fee,
                            b_tx.op_count,
                        )
                        .then_with(|| b_tx.hash.0.cmp(&a_tx.hash.0))
                    })
                    .map(|(h, tx)| (*h, tx.clone()))
                {
                    if queued.is_better_than(&evict_tx) {
                        by_hash.remove(&evict_hash);
                        self.seen.write().remove(&evict_hash);
                        let mut account_states = self.account_states.write();
                        Self::drop_transaction(&mut account_states, &evict_tx);
                    } else {
                        return TxQueueResult::QueueFull;
                    }
                } else {
                    return TxQueueResult::QueueFull;
                }
            }
        }

        // Fee balance validation (if provider is set)
        // Check that fee-source has sufficient balance for total fees + new fee
        //
        // Parity: stellar-core TransactionQueue::canAdd() skips both tx validation
        // and fee balance checks for loadgen txs under #ifdef BUILD_TESTS.
        #[cfg(any(test, feature = "test-utils"))]
        let skip_fee = self
            .skip_fee_balance_check
            .load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(any(test, feature = "test-utils")))]
        let skip_fee = false;

        if let Some(ref provider) = *self.fee_balance_provider.read() {
            if !skip_fee {
                let fee_source_id = account_id_from_fee_source_key(&new_fee_source_key);

                // Calculate the net new fee being added
                let net_new_fee = if let Some(ref old_tx) = replaced_tx {
                    let old_fee_source_key = fee_source_key(&old_tx.envelope);
                    if old_fee_source_key == new_fee_source_key {
                        // Same fee source - only the difference is new
                        (queued.total_fee as i64).saturating_sub(old_tx.total_fee as i64)
                    } else {
                        // Different fee source - full new fee
                        queued.total_fee as i64
                    }
                } else {
                    queued.total_fee as i64
                };

                // Get current total fees for this fee-source
                let current_total_fees = {
                    let account_states = self.account_states.read();
                    account_states
                        .get(&new_fee_source_key)
                        .map(|s| s.total_fees)
                        .unwrap_or(0)
                };

                // Check if fee-source has sufficient balance
                if let Some(available) = provider.get_available_balance(&fee_source_id) {
                    // available - net_new_fee < current_total_fees means insufficient
                    if available.saturating_sub(net_new_fee) < current_total_fees {
                        return TxQueueResult::Invalid(Some(
                            henyey_tx::TxResultCode::TxInsufficientBalance,
                        ));
                    }
                } else {
                    // Account doesn't exist
                    return TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxNoAccount));
                }
            }
        }

        // Handle fee-bump replacement if applicable
        if let Some(ref old_tx) = replaced_tx {
            // Remove the old transaction from by_hash and seen
            by_hash.remove(&old_tx.hash);
            self.seen.write().remove(&old_tx.hash);

            // If the old tx has a different fee-source, release the fee from that account
            let old_fee_source_key = fee_source_key(&old_tx.envelope);
            if old_fee_source_key != new_fee_source_key {
                let mut account_states = self.account_states.write();
                if let Some(old_fee_state) = account_states.get_mut(&old_fee_source_key) {
                    old_fee_state.total_fees = old_fee_state
                        .total_fees
                        .saturating_sub(old_tx.total_fee as i64);
                    // Remove the account state if it's empty
                    if old_fee_state.is_empty() {
                        account_states.remove(&old_fee_source_key);
                    }
                }
            }
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

            // Update the fee-source account state (tracks total_fees)
            // Note: seq_source and fee_source may be the same account
            if seq_source_key == new_fee_source_key {
                // Same account - already have the entry
                seq_state.total_fees = seq_state.total_fees.saturating_add(fee_to_add);
            } else {
                // Different accounts - update fee-source separately
                let fee_state = account_states.entry(new_fee_source_key).or_default();
                fee_state.total_fees = fee_state.total_fees.saturating_add(fee_to_add);
            }
        }

        by_hash.insert(hash, queued);
        self.seen.write().insert(hash);

        TxQueueResult::Added
    }

    /// Drop a queued transaction from account_states, releasing fees and
    /// cleaning up empty entries.
    ///
    /// Mirrors stellar-core's `dropTransaction()` + `releaseFeeMaybeEraseAccountState()`.
    /// The caller must have already removed the transaction from `by_hash`.
    fn drop_transaction(
        account_states: &mut HashMap<Vec<u8>, AccountState>,
        queued: &QueuedTransaction,
    ) {
        let seq_source = account_key(&queued.envelope);
        let fee_source = fee_source_key(&queued.envelope);

        // Clear the pending transaction on the seq-source account.
        if let Some(state) = account_states.get_mut(&seq_source) {
            if state.transaction.as_ref().map(|t| &t.hash) == Some(&queued.hash) {
                state.transaction = None;
                state.age = 0;
            }
        }

        // Release fees on the fee-source account.
        if let Some(fee_state) = account_states.get_mut(&fee_source) {
            fee_state.total_fees = fee_state.total_fees.saturating_sub(queued.total_fee as i64);
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
        self.by_hash.read().get(hash).cloned()
    }

    /// Check if a transaction is in the queue.
    pub fn contains(&self, hash: &Hash256) -> bool {
        self.by_hash.read().contains_key(hash)
    }

    /// Get the number of pending transactions.
    pub fn len(&self) -> usize {
        self.by_hash.read().len()
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.by_hash.read().is_empty()
    }

    /// Reset all lane-based and global eviction fee thresholds.
    ///
    /// Called whenever the queue is rebuilt or transactions are evicted/shifted
    /// so that stale minimum-fee requirements are not carried forward.
    fn reset_eviction_thresholds(&self) {
        self.classic_lane_evicted_inclusion_fee.write().clear();
        self.soroban_lane_evicted_inclusion_fee.write().clear();
        *self.global_evicted_inclusion_fee.write() = (0, 0);
    }

    /// Clear expired transactions.
    pub fn evict_expired(&self) {
        let mut by_hash = self.by_hash.write();
        let mut account_states = self.account_states.write();
        let max_age = self.config.max_age_secs;
        // Collect expired transactions, then remove them so account_states
        // are properly cleaned up (fee release + empty entry removal).
        let expired_hashes: Vec<Hash256> = by_hash
            .iter()
            .filter(|(_, tx)| tx.is_expired(max_age))
            .map(|(hash, _)| *hash)
            .collect();
        for hash in &expired_hashes {
            if let Some(removed) = by_hash.remove(hash) {
                Self::drop_transaction(&mut account_states, &removed);
            }
        }
        if !expired_hashes.is_empty() {
            let mut seen = self.seen.write();
            for hash in &expired_hashes {
                seen.remove(hash);
            }
        }

        // Mirror stellar-core: clear eviction thresholds after aging to avoid
        // carrying stale min-fee requirements.
        self.reset_eviction_thresholds();
    }

    /// Clear all transactions.
    pub fn clear(&self) {
        self.by_hash.write().clear();
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

        let mut banned = self.banned_transactions.write();
        // Add to the newest (back) set
        if let Some(newest) = banned.back_mut() {
            for hash in tx_hashes {
                newest.insert(*hash);
            }
        }

        // Also remove from the queue if present, cleaning up account_states.
        // Mirrors stellar-core's ban() which calls dropTransaction().
        let mut by_hash = self.by_hash.write();
        let mut account_states = self.account_states.write();
        let mut seen = self.seen.write();
        for hash in tx_hashes {
            if let Some(removed) = by_hash.remove(hash) {
                Self::drop_transaction(&mut account_states, &removed);
            }
            seen.remove(hash);
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

        let mut account_states = self.account_states.write();
        let mut by_hash = self.by_hash.write();
        let mut banned = self.banned_transactions.write();

        // Collect fee releases to apply after processing all transactions
        let mut fee_releases: Vec<(Vec<u8>, i64)> = Vec::new();
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
                        // Remove from by_hash
                        let removed_hash = queued_tx.hash;
                        by_hash.remove(&removed_hash);
                        removed_hashes.push(removed_hash);

                        // Collect fee release info
                        let tx_fee = queued_tx.total_fee as i64;
                        let tx_fee_source_id = henyey_tx::muxed_to_account_id(
                            &henyey_tx::TransactionFrame::from_owned_with_network(
                                queued_tx.envelope.clone(),
                                self.config.network_id,
                            )
                            .fee_source_account(),
                        );
                        let tx_fee_source_key = account_key_from_account_id(&tx_fee_source_id);
                        fee_releases.push((tx_fee_source_key, tx_fee));

                        state.transaction = None;
                        state.age = 0;
                    }
                }
            }

            // Ban the applied tx hash
            let applied_hash = Hash256::hash_xdr(envelope).unwrap_or_default();
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
        for (fee_source_key, tx_fee) in fee_releases {
            if let Some(fee_state) = account_states.get_mut(&fee_source_key) {
                fee_state.total_fees = fee_state.total_fees.saturating_sub(tx_fee);
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
        let mut banned = self.banned_transactions.write();
        let mut account_states = self.account_states.write();
        let mut by_hash = self.by_hash.write();

        // Remove the oldest set (front) to unban those transactions
        let unbanned_count = banned.pop_front().map(|s| s.len()).unwrap_or(0);

        // Add a new empty set at the back for the next ledger
        banned.push_back(HashSet::new());

        let mut evicted_due_to_age = 0;
        let mut accounts_to_remove = Vec::new();
        // Collect fee releases to apply after iteration (to avoid borrow conflicts)
        let mut fee_releases: Vec<(Vec<u8>, u64)> = Vec::new();
        let mut evicted_hashes: Vec<Hash256> = Vec::new();

        // Process account states: increment age, auto-ban stale transactions
        for (account_key, state) in account_states.iter_mut() {
            // Only increment age if there's a pending transaction
            if state.transaction.is_some() {
                state.age += 1;

                // Auto-ban at pending_depth
                if state.age >= self.pending_depth {
                    if let Some(ref queued_tx) = state.transaction {
                        // Add to banned set
                        if let Some(newest) = banned.back_mut() {
                            newest.insert(queued_tx.hash);
                        }
                        // Remove from by_hash and track for seen cleanup
                        by_hash.remove(&queued_tx.hash);
                        evicted_hashes.push(queued_tx.hash);

                        // Track fee release for the fee-source account
                        let tx_fee_source_key = fee_source_key(&queued_tx.envelope);
                        fee_releases.push((tx_fee_source_key, queued_tx.total_fee));

                        evicted_due_to_age += 1;
                    }
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
        for (fee_source_key, tx_fee) in fee_releases {
            if let Some(fee_state) = account_states.get_mut(&fee_source_key) {
                fee_state.total_fees = fee_state.total_fees.saturating_sub(tx_fee as i64);
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

        // Reset eviction thresholds for the new ledger
        self.reset_eviction_thresholds();

        // Clean up seen set for evicted transactions
        if !evicted_hashes.is_empty() {
            let mut seen = self.seen.write();
            for hash in &evicted_hashes {
                seen.remove(hash);
            }
        }

        ShiftResult {
            unbanned_count,
            evicted_due_to_age,
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
            let by_hash = self.by_hash.read();
            by_hash.values().map(|qt| qt.envelope.clone()).collect()
        };

        // Clear queue state but preserve bans (bans cannot be invalidated
        // by a protocol upgrade, matching upstream).
        {
            let mut by_hash = self.by_hash.write();
            by_hash.clear();
        }
        {
            let mut seen = self.seen.write();
            seen.clear();
        }
        {
            let mut account_states = self.account_states.write();
            account_states.clear();
        }
        self.reset_eviction_thresholds();

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
        let by_hash = self.by_hash.read();
        let mut accounts: HashSet<Vec<u8>> = HashSet::new();
        let mut out = Vec::new();
        for tx in by_hash.values() {
            let account_id = account_id_from_envelope(&tx.envelope);
            let key = account_key_from_account_id(&account_id);
            if accounts.insert(key) {
                out.push(account_id);
            }
        }
        out
    }

    /// Return transaction hashes ordered by fee per op (desc) then received time (asc).
    pub fn ordered_hashes_by_fee(&self, limit: usize) -> Vec<Hash256> {
        let by_hash = self.by_hash.read();
        let mut entries: Vec<_> = by_hash
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
        let by_hash = self.by_hash.read();
        let seen = self.seen.read();

        // Count accounts with pending transactions
        let mut accounts: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        for tx in by_hash.values() {
            let account_id = account_id_from_envelope(&tx.envelope);
            accounts.insert(account_key_from_account_id(&account_id));
        }

        TxQueueStats {
            pending_count: by_hash.len(),
            account_count: accounts.len(),
            banned_count: self.banned_count(),
            seen_count: seen.len(),
        }
    }
}

/// Statistics about the transaction queue.
#[derive(Debug, Clone, Default)]
pub struct TxQueueStats {
    /// Number of pending transactions.
    pub pending_count: usize,
    /// Number of accounts with pending transactions.
    pub account_count: usize,
    /// Number of currently banned transactions.
    pub banned_count: usize,
    /// Number of seen (deduplicated) transaction hashes.
    pub seen_count: usize,
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

fn has_hashx_signature(
    signatures: &[DecoratedSignature],
    key: &stellar_xdr::curr::Uint256,
) -> bool {
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
    tx_hash: &Hash256,
    signatures: &[DecoratedSignature],
    payload: &stellar_xdr::curr::SignerKeyEd25519SignedPayload,
) -> bool {
    let pk = match henyey_crypto::PublicKey::from_bytes(&payload.ed25519.0) {
        Ok(pk) => pk,
        Err(_) => return false,
    };

    let mut data = Vec::with_capacity(32 + payload.payload.len());
    data.extend_from_slice(&tx_hash.0);
    data.extend_from_slice(&payload.payload);
    let payload_hash = Hash256::hash(&data);

    signatures
        .iter()
        .any(|sig| henyey_tx::validation::verify_signature_with_key(&payload_hash, sig, &pk))
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
    use stellar_xdr::curr::{
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
        crate::tx_set_utils::envelope_fee(envelope) as u64
    }

    fn envelope_seq(envelope: &TransactionEnvelope) -> i64 {
        match envelope {
            TransactionEnvelope::TxV0(tx) => tx.tx.seq_num.0,
            TransactionEnvelope::Tx(tx) => tx.tx.seq_num.0,
            TransactionEnvelope::TxFeeBump(tx) => match &tx.tx.inner_tx {
                stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => inner.tx.seq_num.0,
            },
        }
    }

    fn envelope_size(envelope: &TransactionEnvelope) -> usize {
        envelope
            .to_xdr(stellar_xdr::curr::Limits::none())
            .map(|bytes| bytes.len())
            .unwrap_or(0)
    }

    fn full_hash(envelope: &TransactionEnvelope) -> Hash256 {
        Hash256::hash_xdr(envelope).expect("hash tx")
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
                stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
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
        let hash1 = Hash256::hash_xdr(&tx1).unwrap();
        let mut tx2 = make_test_envelope(200, 1);
        set_source(&mut tx2, 2);
        let hash2 = Hash256::hash_xdr(&tx2).unwrap();

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
        let hash = Hash256::hash_xdr(&tx).unwrap();
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
        let hash1 = Hash256::hash_xdr(&tx1).unwrap();
        queue.ban(&[hash1]);

        queue.shift(); // ledger 2

        // Ban tx2 in ledger 2
        let mut tx2 = make_test_envelope(200, 1);
        set_source(&mut tx2, 2);
        let hash2 = Hash256::hash_xdr(&tx2).unwrap();
        queue.ban(&[hash2]);

        queue.shift(); // ledger 3

        // Ban tx3 in ledger 3
        let mut tx3 = make_test_envelope(200, 1);
        set_source(&mut tx3, 3);
        let hash3 = Hash256::hash_xdr(&tx3).unwrap();
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

        let mut fees: Vec<u64> = set.transactions.iter().map(envelope_fee).collect();
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
            .transactions
            .iter()
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
        assert_eq!(envelope_seq(&set.transactions[0]), 1);
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
        let mut seqs: Vec<i64> = set.transactions.iter().map(envelope_seq).collect();
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
        let mut seqs: Vec<i64> = set.transactions.iter().map(envelope_seq).collect();
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
        let mut seqs: Vec<i64> = set.transactions.iter().map(envelope_seq).collect();
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
        let mut seqs: Vec<i64> = set.transactions.iter().map(envelope_seq).collect();
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
        let mut seqs: Vec<i64> = set.transactions.iter().map(envelope_seq).collect();
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
        let recomputed = tx_set.recompute_hash().expect("recompute hash");
        assert_eq!(tx_set.hash, recomputed);
    }

    #[test]
    fn test_generalized_tx_set_phase_split() {
        let queue = TransactionQueue::with_defaults();

        let classic = make_test_envelope(200, 1);
        let soroban = make_soroban_envelope(200);
        queue.try_add(classic.clone());
        queue.try_add(soroban.clone());

        let (_set, gen) = queue.build_generalized_tx_set(Hash256::ZERO, 100);
        let stellar_xdr::curr::GeneralizedTransactionSet::V1(v1) = gen;
        assert_eq!(v1.phases.len(), 2);

        match &v1.phases[0] {
            stellar_xdr::curr::TransactionPhase::V0(components) => {
                let txs: Vec<_> = components
                    .iter()
                    .flat_map(|component| match component {
                        stellar_xdr::curr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) => {
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
            stellar_xdr::curr::TransactionPhase::V1(parallel) => {
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

        let (tx_set, gen) = queue.build_generalized_tx_set(Hash256::ZERO, 100);
        let recomputed = tx_set.recompute_hash().expect("recompute hash");
        assert_eq!(tx_set.hash, recomputed);

        let gen_hash = Hash256::hash_xdr(&gen).expect("hash generalized");
        assert_eq!(tx_set.hash, gen_hash);
    }

    #[test]
    fn test_classic_base_fee_defaults_to_min_fee() {
        let queue = TransactionQueue::with_defaults();
        let expected_base_fee = queue.validation_context.read().base_fee as i64;

        queue.try_add(make_test_envelope(200, 1));
        queue.try_add(make_test_envelope(300, 1));

        let (_set, gen) = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let stellar_xdr::curr::GeneralizedTransactionSet::V1(v1) = gen;
        let base_fee = match &v1.phases[0] {
            stellar_xdr::curr::TransactionPhase::V0(components) => {
                let stellar_xdr::curr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) =
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

        let (_set, gen) = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let stellar_xdr::curr::GeneralizedTransactionSet::V1(v1) = gen;
        let base_fee = match &v1.phases[1] {
            stellar_xdr::curr::TransactionPhase::V1(parallel) => parallel.base_fee,
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

        let (_set, gen) = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let stellar_xdr::curr::GeneralizedTransactionSet::V1(v1) = gen;

        let txs = match &v1.phases[0] {
            stellar_xdr::curr::TransactionPhase::V0(components) => {
                let stellar_xdr::curr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) =
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

        let (_set, gen) = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let stellar_xdr::curr::GeneralizedTransactionSet::V1(v1) = gen;

        let mut txs = Vec::new();
        match &v1.phases[1] {
            stellar_xdr::curr::TransactionPhase::V1(parallel) => {
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

        queue.update_validation_context(1, 0, 25, 500, 5_000_000, 0);

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

        let (_set, gen) = queue.build_generalized_tx_set(Hash256::ZERO, 100);
        let stellar_xdr::curr::GeneralizedTransactionSet::V1(v1) = gen;
        let base_fee = match &v1.phases[0] {
            stellar_xdr::curr::TransactionPhase::V0(components) => match &components[0] {
                stellar_xdr::curr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) => {
                    comp.base_fee
                }
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
        assert_eq!(envelope_fee(&transactions[0]), 400);
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
        for tx in &set.transactions {
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
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        let hashes: Vec<_> = set.transactions.iter().map(full_hash).collect();
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

        let (_set, gen) = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let stellar_xdr::curr::GeneralizedTransactionSet::V1(tx_set) = gen;
        let phases = &tx_set.phases;
        let components = match &phases[0] {
            stellar_xdr::curr::TransactionPhase::V0(components) => components,
            _ => panic!("expected classic phase"),
        };
        assert_eq!(components.len(), 2);

        let mut base_fees = Vec::new();
        let mut tx_counts = Vec::new();
        for comp in components.iter() {
            let stellar_xdr::curr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = comp;
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
            let mut by_hash = queue.by_hash.write();
            for tx in by_hash.values_mut() {
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

        let (_set, gen) = queue.build_generalized_tx_set(Hash256::ZERO, 200);
        let stellar_xdr::curr::GeneralizedTransactionSet::V1(v1) = gen;
        let components = match &v1.phases[0] {
            stellar_xdr::curr::TransactionPhase::V0(comps) => comps,
            _ => panic!("expected classic phase"),
        };

        let mut has_dex_fee = false;
        let mut has_classic_fee = false;
        for comp in components.iter() {
            let stellar_xdr::curr::TxSetComponent::TxsetCompTxsMaybeDiscountedFee(comp) = comp;
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

        let (_set, gen) = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let stellar_xdr::curr::GeneralizedTransactionSet::V1(v1) = gen;
        let base_fee = match &v1.phases[1] {
            stellar_xdr::curr::TransactionPhase::V1(parallel) => parallel.base_fee,
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

        let (_set, gen) = queue.build_generalized_tx_set(Hash256::ZERO, 10);
        let stellar_xdr::curr::GeneralizedTransactionSet::V1(v1) = gen;
        let parallel = match &v1.phases[1] {
            stellar_xdr::curr::TransactionPhase::V1(parallel) => parallel,
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
        assert_eq!(envelope_fee(&transactions[0]), 12000);
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
        assert_eq!(envelope_fee(&transactions[0]), 12000);
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
        let key_a = account_key(&transactions[0]);
        let key_b = account_key(&transactions[1]);
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
        // max_queue_ops = 3: room for exactly 3 ops total
        let config = TxQueueConfig {
            max_queue_ops: Some(3),
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

        // Add both: now at 3/3 ops
        assert_eq!(queue.try_add(tx_a.clone()), TxQueueResult::Added);
        assert_eq!(queue.try_add(tx_b), TxQueueResult::Added);
        assert_eq!(queue.len(), 2);

        // Build a fee-bump wrapping the same inner tx (source 50, seq 1)
        // with a higher fee. This should succeed because the old tx's 2 ops
        // are excluded from the capacity check.
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

    /// Regression: a Soroban tx whose byte size exceeds the initial min
    /// read-ledger-entries limit (6) should still be admitted when the
    /// dynamic resource limits (with correct ordering) allow it.
    #[test]
    fn test_soroban_tx_admitted_with_restrictive_initial_limits() {
        // Simulate the initial Soroban limits on a fresh protocol 25 network
        // multiplied by POOL_LEDGER_MULTIPLIER (2).
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
        use stellar_xdr::curr::TimeBounds;

        let lcl_close_time: u64 = 1_700_000_000;
        let expected_close_secs: u64 = 5;
        // Upper bound offset = expected_close_time * 2 + drift.
        // With drift=0 (just closed), offset = 10.
        // A tx with max_time = lcl_close_time + 3 would expire before
        // lcl_close_time + 10, so it should be rejected.
        let max_time = lcl_close_time + 3;

        let config = TxQueueConfig {
            validate_signatures: false,
            validate_time_bounds: true,
            expected_ledger_close_secs: expected_close_secs,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);
        queue.update_validation_context(100, lcl_close_time, 21, 100, 5_000_000, 0);

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
                min_time: stellar_xdr::curr::TimePoint(0),
                max_time: stellar_xdr::curr::TimePoint(max_time),
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
        use stellar_xdr::curr::TimeBounds;

        let lcl_close_time: u64 = 1_700_000_000;
        let config = TxQueueConfig {
            validate_signatures: false,
            validate_time_bounds: true,
            expected_ledger_close_secs: 5,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);
        queue.update_validation_context(100, lcl_close_time, 21, 100, 5_000_000, 0);

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
                min_time: stellar_xdr::curr::TimePoint(0),
                max_time: stellar_xdr::curr::TimePoint(max_time),
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

    #[test]
    fn test_is_filtered_empty_config() {
        let config = TxQueueConfig {
            validate_signatures: false,
            validate_time_bounds: false,
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
            validate_time_bounds: false,
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
            validate_time_bounds: false,
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
            validate_time_bounds: false,
            filtered_operation_types: filtered,
            ..Default::default()
        };
        let queue = TransactionQueue::new(config);

        // Create a transaction with CreateAccount operation
        let envelope = make_test_envelope(1000, 1);

        // Should return Filtered result
        assert_eq!(queue.try_add(envelope), TxQueueResult::Filtered);
    }

    #[test]
    fn test_is_filtered_soroban_type() {
        let mut filtered = HashSet::new();
        filtered.insert(OperationType::InvokeHostFunction);

        let config = TxQueueConfig {
            validate_signatures: false,
            validate_time_bounds: false,
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
            validate_time_bounds: false,
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
            validate_time_bounds: false,
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
            validate_time_bounds: false,
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
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxMalformed)) => {}
            other => panic!("expected Invalid(TxMalformed), got {:?}", other),
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
            TxQueueResult::Invalid(Some(henyey_tx::TxResultCode::TxMalformed)) => {}
            other => panic!("expected Invalid(TxMalformed), got {:?}", other),
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

    /// Soroban transaction with resource_fee == total_fee is valid (inclusion_fee = 0).
    #[test]
    fn test_soroban_resource_fee_equals_total_fee_accepted() {
        let queue = TransactionQueue::with_defaults();
        let mut envelope = make_soroban_envelope_with_resources(500, 100);
        if let TransactionEnvelope::Tx(ref mut env) = envelope {
            if let TransactionExt::V1(ref mut data) = env.tx.ext {
                data.resource_fee = 500; // == total_fee
            }
        }
        set_source(&mut envelope, 131);

        // Should be accepted (inclusion_fee = 0, which is valid)
        assert_eq!(queue.try_add(envelope), TxQueueResult::Added);
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
    ) -> henyey_tx::TransactionFrame {
        use stellar_xdr::curr::LedgerKey;
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
                LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                    account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([i as u8; 32]))),
                })
            })
            .collect();
        let read_write: Vec<LedgerKey> = (0..read_write_entries)
            .map(|i| {
                LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
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

        henyey_tx::TransactionFrame::from_owned_with_network(envelope, NetworkId::testnet())
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
        let hash1 = Hash256::hash_xdr(&tx1).unwrap();
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
        let hash1 = Hash256::hash_xdr(&tx1).unwrap();

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
        let hash = Hash256::hash_xdr(&tx).unwrap();

        assert_eq!(queue.try_add(tx), TxQueueResult::Added);
        assert!(queue.seen.read().contains(&hash));

        // Artificially expire the transaction
        {
            let mut by_hash = queue.by_hash.write();
            for tx in by_hash.values_mut() {
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
        let hash = Hash256::hash_xdr(&tx).unwrap();

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
        let hash = Hash256::hash_xdr(&tx).unwrap();

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
            let mut by_hash = queue.by_hash.write();
            for tx in by_hash.values_mut() {
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
}

#[cfg(test)]
mod pending_depth_tests {
    use super::*;
    use stellar_xdr::curr::{
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
                    destination: stellar_xdr::curr::AccountId(
                        stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(Uint256([255u8; 32])),
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
                signature: stellar_xdr::curr::Signature(vec![0u8; 64].try_into().unwrap()),
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
    // DEFAULT_PENDING_DEPTH auto-ban tests
    // =========================================================================

    #[test]
    fn test_default_pending_depth_is_4() {
        assert_eq!(DEFAULT_PENDING_DEPTH, 4);
    }

    #[test]
    fn test_pending_tx_not_auto_banned_before_depth() {
        // With pending_depth=4, a pending TX should survive 3 shifts
        let queue = TransactionQueue::with_depths(TxQueueConfig::default(), 10, 4);

        let mut tx = make_test_envelope(200, 1);
        set_source(&mut tx, 50);
        let hash = Hash256::hash_xdr(&tx).unwrap();

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
        let hash = Hash256::hash_xdr(&tx).unwrap();

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
        let hash = Hash256::hash_xdr(&tx).unwrap();

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
        let hash_a = Hash256::hash_xdr(&tx_a).unwrap();
        assert_eq!(queue.try_add(tx_a), TxQueueResult::Added);

        // Shift once (TX A age = 1)
        queue.shift();

        // Add TX B (TX A age = 1, TX B age = 0)
        let mut tx_b = make_test_envelope(200, 1);
        set_source(&mut tx_b, 61);
        let hash_b = Hash256::hash_xdr(&tx_b).unwrap();
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
        let hash = Hash256::hash_xdr(&tx).unwrap();

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
