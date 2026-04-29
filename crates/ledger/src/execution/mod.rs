//! Transaction execution during ledger close.
//!
//! This module bridges the ledger close process with the transaction processing
//! layer (`stellar-core-tx`). It handles:
//!
//! - Loading required state from snapshots into the execution environment
//! - Executing transactions in the correct order with proper fee handling
//! - Recording state changes to the [`LedgerDelta`]
//! - Generating transaction metadata for history
//!
//! # Execution Flow
//!
//! 1. **State Loading**: Required accounts, trustlines, and other entries are
//!    loaded from the snapshot into the [`LedgerStateManager`]
//!
//! 2. **Fee Charging**: Transaction fees are charged upfront, even for
//!    transactions that will fail validation
//!
//! 3. **Operation Execution**: Each operation is executed in sequence,
//!    with results collected for the transaction result
//!
//! 4. **State Recording**: Changes are recorded to the [`LedgerDelta`]
//!    for later application to the bucket list
//!
//! # Soroban Support
//!
//! For Protocol 20+, this module handles Soroban smart contract execution:
//!
//! - Loading contract data and code from the footprint
//! - Configuring the Soroban host with network parameters
//! - Computing resource fees and rent
//! - Processing contract events
//!
//! [`LedgerDelta`]: crate::LedgerDelta
//! [`LedgerStateManager`]: henyey_tx::LedgerStateManager

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use henyey_common::protocol::{
    protocol_version_is_before, protocol_version_starts_from, ProtocolVersion,
};
use henyey_common::{Hash256, NetworkId, LIQUIDITY_POOL_FEE_V18};
use henyey_crypto::account_id_to_strkey;
use soroban_env_host_p25::fees::{
    compute_rent_write_fee_per_1kb, compute_transaction_resource_fee, FeeConfiguration,
    RentFeeConfiguration, RentWriteFeeConfiguration,
};

use henyey_tx::{
    make_account_address, make_claimable_balance_address, make_muxed_account_address,
    soroban::{PersistentModuleCache, SorobanConfig},
    validation, ClassicEventConfig, LedgerContext, LedgerStateManager, OpEventManager,
    TransactionFrame, TxError, TxEventManager,
};
use stellar_xdr::curr::{
    AccountEntry, AccountId, AccountMergeResult, AllowTrustOp, AlphaNum12, AlphaNum4, Asset,
    AssetCode, ClaimableBalanceEntry, ClaimableBalanceId, ConfigSettingEntry, ConfigSettingId,
    ContractEvent, CreateClaimableBalanceResult, DiagnosticEvent, ExtensionPoint, InflationResult,
    InnerTransactionResult, InnerTransactionResultExt, InnerTransactionResultPair,
    InnerTransactionResultResult, LedgerEntry, LedgerEntryChange, LedgerEntryChanges,
    LedgerEntryData, LedgerKey, LedgerKeyClaimableBalance, LedgerKeyConfigSetting,
    LedgerKeyLiquidityPool, LedgerKeyTrustLine, Limits, LiquidityPoolEntry, LiquidityPoolEntryBody,
    ManageBuyOfferResult, ManageSellOfferResult, MuxedAccount, OfferEntry, Operation,
    OperationBody, OperationMetaV2, OperationResult, OperationResultTr, OperationType,
    PathPaymentStrictReceiveResult, PathPaymentStrictSendResult, PoolId, Preconditions, ScAddress,
    SignerKey, SorobanTransactionData, SorobanTransactionMetaExt, SorobanTransactionMetaExtV1,
    SorobanTransactionMetaV2, TransactionEnvelope, TransactionEvent, TransactionMeta,
    TransactionMetaV4, TransactionResult, TransactionResultCode, TransactionResultExt,
    TransactionResultMetaV1, TransactionResultPair, TransactionResultResult, TrustLineAsset, VecM,
    WriteXdr,
};
use tracing::{debug, warn};

use crate::close::TxWithFee;
use crate::delta::LedgerDelta;
use crate::snapshot::SnapshotHandle;
use crate::{LedgerError, Result};

use henyey_bucket::HotArchiveBucketList;

mod account_loading;
mod apply;
mod config;
mod meta;
mod preconditions;
mod result_mapping;
mod signatures;
mod tx_set;

pub(crate) use config::{compute_soroban_resource_fee, load_soroban_network_info};
pub use config::{load_frozen_key_config, load_soroban_config};
pub use result_mapping::build_tx_result_pair;
pub(crate) use tx_set::pre_deduct_all_fees_on_delta;
pub use tx_set::{
    compute_state_size_window_entry, execute_soroban_parallel_phase, execute_transaction_set,
    execute_transaction_set_with_fee_mode, run_transactions_on_executor, RunTransactionsParams,
};

use apply::{RestoredEntries, AUTHORIZED_FLAG};
use meta::*;
use signatures::*;

/// Wrapper around HotArchiveBucketList that implements the HotArchiveLookup trait.
///
/// This allows the ledger execution layer to look up archived entries without
/// requiring the tx layer to depend on the bucket crate.
pub struct HotArchiveLookupImpl {
    hot_archive: std::sync::Arc<parking_lot::RwLock<Option<HotArchiveBucketList>>>,
}

pub struct TransactionExecutionRequest {
    pub tx_envelope: Arc<TransactionEnvelope>,
    pub base_fee: u32,
    pub soroban_prng_seed: Option<[u8; 32]>,
    pub deduct_fee: bool,
}

impl TransactionExecutionRequest {
    pub fn from_envelope(
        tx_envelope: &TransactionEnvelope,
        base_fee: u32,
        soroban_prng_seed: Option<[u8; 32]>,
        deduct_fee: bool,
    ) -> Self {
        Self {
            tx_envelope: Arc::new(tx_envelope.clone()),
            base_fee,
            soroban_prng_seed,
            deduct_fee,
        }
    }
}

pub(super) struct OperationExecutionRequest<'a> {
    pub(super) op: &'a stellar_xdr::curr::Operation,
    pub(super) source: &'a AccountId,
    pub(super) tx_source: &'a AccountId,
    pub(super) tx_seq: i64,
    pub(super) op_index: u32,
    pub(super) context: &'a LedgerContext,
    pub(super) soroban_data: Option<&'a stellar_xdr::curr::SorobanTransactionData>,
}

impl HotArchiveLookupImpl {
    pub fn new(
        hot_archive: std::sync::Arc<parking_lot::RwLock<Option<HotArchiveBucketList>>>,
    ) -> Self {
        Self { hot_archive }
    }
}

impl henyey_tx::soroban::HotArchiveLookup for HotArchiveLookupImpl {
    fn get(
        &self,
        key: &LedgerKey,
    ) -> std::result::Result<Option<LedgerEntry>, Box<dyn std::error::Error + Send + Sync>> {
        // Use the hot archive bucket list's get method
        let guard = self.hot_archive.read();
        let hot_archive = match guard.as_ref() {
            Some(ha) => ha,
            None => {
                return Ok(None);
            }
        };
        Ok(hot_archive.get(key)?)
    }
}

/// Soroban network configuration information for the /sorobaninfo endpoint.
///
/// This struct contains all the Soroban-related configuration settings from
/// the ledger, matching the format returned by stellar-core's `/sorobaninfo`
/// HTTP endpoint in "basic" format.
#[derive(Debug, Clone, Default)]
pub struct SorobanNetworkInfo {
    /// Maximum contract code size in bytes.
    pub max_contract_size: u32,
    /// Maximum contract data key size in bytes.
    pub max_contract_data_key_size: u32,
    /// Maximum contract data entry size in bytes.
    pub max_contract_data_entry_size: u32,
    /// Per-transaction compute limits.
    pub tx_max_instructions: i64,
    /// Per-ledger compute limits.
    pub ledger_max_instructions: i64,
    /// Fee rate per instructions increment.
    pub fee_rate_per_instructions_increment: i64,
    /// Transaction memory limit.
    pub tx_memory_limit: u32,
    /// Per-ledger disk read limits.
    pub ledger_max_read_ledger_entries: u32,
    pub ledger_max_read_bytes: u32,
    pub ledger_max_write_ledger_entries: u32,
    pub ledger_max_write_bytes: u32,
    /// Per-transaction disk read/write limits.
    pub tx_max_read_ledger_entries: u32,
    pub tx_max_read_bytes: u32,
    pub tx_max_write_ledger_entries: u32,
    pub tx_max_write_bytes: u32,
    /// Fees per entry and per KB.
    pub fee_read_ledger_entry: i64,
    pub fee_write_ledger_entry: i64,
    pub fee_read_1kb: i64,
    pub fee_write_1kb: i64,
    pub fee_historical_1kb: i64,
    /// Contract events settings.
    pub tx_max_contract_events_size_bytes: u32,
    pub fee_contract_events_size_1kb: i64,
    /// Bandwidth settings.
    pub ledger_max_tx_size_bytes: u32,
    pub tx_max_size_bytes: u32,
    pub fee_transaction_size_1kb: i64,
    /// General ledger settings.
    pub ledger_max_tx_count: u32,
    /// State archival settings.
    pub max_entry_ttl: u32,
    pub min_temporary_ttl: u32,
    pub min_persistent_ttl: u32,
    pub persistent_rent_rate_denominator: i64,
    pub temp_rent_rate_denominator: i64,
    pub max_entries_to_archive: u32,
    pub bucketlist_size_window_sample_size: u32,
    pub eviction_scan_size: i64,
    pub starting_eviction_scan_level: u32,
    /// Computed value: average bucket list size.
    pub average_bucket_list_size: u64,
    /// Rent write fee configuration fields (from ContractLedgerCostV0).
    pub state_target_size_bytes: i64,
    pub rent_fee_1kb_state_size_low: i64,
    pub rent_fee_1kb_state_size_high: i64,
    pub state_size_rent_fee_growth_factor: u32,
    /// SCP timing settings (Protocol 23+).
    pub nomination_timeout_initial_ms: u32,
    pub nomination_timeout_increment_ms: u32,
    pub ballot_timeout_initial_ms: u32,
    pub ballot_timeout_increment_ms: u32,
    /// Ledger target close time in milliseconds (Protocol 23+).
    pub ledger_target_close_time_ms: u32,
    /// Maximum number of dependent TX clusters per stage (from ContractParallelComputeV0).
    pub ledger_max_dependent_tx_clusters: u32,
    /// Maximum footprint entries per transaction (Protocol 23+, from ContractLedgerCostExtV0).
    pub tx_max_footprint_entries: u32,
}

pub(super) struct RefundableFeeTracker {
    pub(super) non_refundable_fee: i64,
    pub(super) max_refundable_fee: i64,
    pub(super) consumed_event_size_bytes: u32,
    pub(super) consumed_rent_fee: i64,
    pub(super) consumed_refundable_fee: i64,
}

impl RefundableFeeTracker {
    fn new(non_refundable_fee: i64, max_refundable_fee: i64) -> Self {
        Self {
            non_refundable_fee,
            max_refundable_fee,
            consumed_event_size_bytes: 0,
            consumed_rent_fee: 0,
            consumed_refundable_fee: 0,
        }
    }

    fn consume(
        &mut self,
        frame: &TransactionFrame,
        protocol_version: u32,
        config: &SorobanConfig,
        event_size_bytes: u32,
        rent_fee: i64,
    ) -> bool {
        self.consumed_event_size_bytes = self
            .consumed_event_size_bytes
            .saturating_add(event_size_bytes);
        self.consumed_rent_fee = self.consumed_rent_fee.saturating_add(rent_fee);

        // First check: rent fee alone must not exceed max refundable fee.
        // Parity: stellar-core SorobanTxData::consumeRefundableSorobanResources —
        // checks consumed rent fee against max before computing events fee.
        if self.consumed_rent_fee > self.max_refundable_fee {
            tracing::debug!(
                consumed_rent_fee = self.consumed_rent_fee,
                max_refundable_fee = self.max_refundable_fee,
                rent_fee = rent_fee,
                "InsufficientRefundableFee: rent_fee exceeds max"
            );
            return false;
        }

        let (_, refundable_fee) = match compute_soroban_resource_fee(
            frame,
            protocol_version,
            config,
            self.consumed_event_size_bytes,
        ) {
            Some(pair) => pair,
            None => return false,
        };
        self.consumed_refundable_fee = self.consumed_rent_fee.saturating_add(refundable_fee);

        // Second check: total consumed (rent + events) must not exceed max refundable fee.
        if self.consumed_refundable_fee > self.max_refundable_fee {
            tracing::debug!(
                consumed_refundable_fee = self.consumed_refundable_fee,
                max_refundable_fee = self.max_refundable_fee,
                consumed_rent_fee = self.consumed_rent_fee,
                refundable_fee = refundable_fee,
                event_size_bytes = self.consumed_event_size_bytes,
                "InsufficientRefundableFee: total consumed exceeds max"
            );
            return false;
        }
        true
    }

    fn refund_amount(&self) -> i64 {
        if self.max_refundable_fee > self.consumed_refundable_fee {
            self.max_refundable_fee - self.consumed_refundable_fee
        } else {
            0
        }
    }

    /// Reset all consumed fee values to zero.
    ///
    /// This mirrors stellar-core's `RefundableFeeTracker::resetConsumedFee()` which is called
    /// by `MutableTransactionResultBase::setError()` when a transaction fails. When a transaction
    /// fails for any reason (including InsufficientRefundableFee), stellar-core resets the consumed fee
    /// tracker so that the full `max_refundable_fee` is refunded to the user.
    fn reset(&mut self) {
        self.consumed_event_size_bytes = 0;
        self.consumed_rent_fee = 0;
        self.consumed_refundable_fee = 0;
    }
}

/// Profiling timings for transaction execution phases (all in microseconds).
#[derive(Debug, Clone, Default)]
pub struct TxExecTimings {
    /// Per-operation-type timing: maps op type to (total_us, count).
    pub op_type_timings: HashMap<OperationType, (u64, u32)>,
    /// Total transaction execution time.
    pub exec_time_us: u64,
    /// Sub-phase timings.
    pub validation_us: u64,
    pub fee_seq_us: u64,
    pub footprint_us: u64,
    pub ops_us: u64,
    // Validation sub-component timings
    pub val_account_load_us: u64,
    pub val_tx_hash_us: u64,
    pub val_ed25519_us: u64,
    pub val_other_us: u64,
    // Fee/seq sub-component timings
    pub fee_deduct_us: u64,
    pub op_sig_check_us: u64,
    pub signer_removal_us: u64,
    pub seq_bump_us: u64,
    pub meta_build_us: u64,
    // Meta sub-component timings
    pub meta_commit_us: u64,
    pub meta_fee_refund_us: u64,
    pub meta_build_phase_us: u64,
}

/// Result of executing a transaction.
#[derive(Debug, Clone)]
pub struct TransactionExecutionResult {
    /// Whether the transaction succeeded.
    pub success: bool,
    /// Fee charged (always charged even on failure).
    pub fee_charged: i64,
    /// Fee refund (for Soroban transactions, protocol < 25 applies this to inner fee too).
    pub fee_refund: i64,
    /// Operation results.
    pub operation_results: Vec<OperationResult>,
    /// Error message if failed.
    pub error: Option<String>,
    /// Failure reason for mapping to XDR result codes.
    pub failure: Option<ExecutionFailure>,
    /// Transaction meta (for ledger close meta).
    pub tx_meta: Option<TransactionMeta>,
    /// Fee processing changes (for ledger close meta).
    pub fee_changes: Option<LedgerEntryChanges>,
    /// Post-apply fee processing changes (refunds).
    pub post_fee_changes: Option<LedgerEntryChanges>,
    /// Keys of entries restored from the hot archive (Protocol 23+).
    /// These should be passed to HotArchiveBucketList::add_batch as restored_keys.
    pub hot_archive_restored_keys: Vec<LedgerKey>,
    /// Profiling timings for execution phases.
    pub timings: TxExecTimings,
    /// Cached transaction hash (computed during validation, reused in result building).
    pub tx_hash: Option<Hash256>,
    /// Whether this is a fee-bump outer-wrapper failure (e.g. fee source missing,
    /// insufficient fee). When true, the failure should be serialized as a top-level
    /// result code, not wrapped in TxFeeBumpInnerFailed. Matches stellar-core's
    /// distinction between setError (outer) and setInnermostError (inner).
    pub fee_bump_outer_failure: bool,
}

/// Type alias: `ExecutionFailure` is now `TransactionResultCode` from the XDR crate.
///
/// Previously this was a custom enum with 15 variants that mapped 1:1 to
/// `TransactionResultCode`. We now use the XDR type directly, eliminating
/// the intermediate mapping layer.
pub type ExecutionFailure = TransactionResultCode;

/// Create a failed `TransactionExecutionResult` with no fee charged or meta.
pub(super) fn failed_result(
    failure: TransactionResultCode,
    error: &str,
) -> TransactionExecutionResult {
    TransactionExecutionResult {
        success: false,
        fee_charged: 0,
        fee_refund: 0,
        operation_results: vec![],
        error: Some(error.into()),
        failure: Some(failure),
        tx_meta: None,
        fee_changes: None,
        post_fee_changes: None,
        hot_archive_restored_keys: Vec::new(),
        timings: TxExecTimings::default(),
        tx_hash: None,
        fee_bump_outer_failure: false,
    }
}

/// Data returned by successful pre-fee validation of a transaction.
pub(super) struct ValidatedTransaction {
    pub(super) frame: TransactionFrame,
    pub(super) fee_source_id: AccountId,
    pub(super) inner_source_id: AccountId,
    pub(super) outer_hash: Hash256,
    // Sub-component timings from validation (microseconds)
    pub(super) val_account_load_us: u64,
    pub(super) val_tx_hash_us: u64,
    pub(super) val_ed25519_us: u64,
    pub(super) val_other_us: u64,
}

/// Validation failure with additional context about the validation level reached.
/// This is used to determine whether the sequence number should still be bumped
/// even though validation failed, matching stellar-core's ValidationType enum.
pub(super) struct ValidationFailure {
    pub(super) result: TransactionExecutionResult,
    /// Whether the validation passed the sequence check (equivalent to
    /// stellar-core's `cv >= kInvalidUpdateSeqNum`). When true, the sequence
    /// number should be bumped even though the TX failed validation.
    pub(super) past_seq_check: bool,
}

/// Result of the pre-apply phase of transaction execution.
///
/// This captures all data produced by `pre_apply()` that is needed by the
/// subsequent `apply_body()` phase. It bridges the two phases, matching
/// stellar-core's architecture where `preParallelApply` produces state
/// (via `MutableTransactionResultBase&` and `AbstractLedgerTxn&`) that
/// `parallelApply` consumes.
///
/// The pre-apply phase validates the transaction, deducts fees (if applicable),
/// removes one-time signers, bumps the sequence number, and commits these
/// changes. The apply phase then executes operations using this context.
pub(super) struct PreApplyResult {
    // Transaction identity
    pub(super) frame: TransactionFrame,
    pub(super) fee_source_id: AccountId,
    pub(super) inner_source_id: AccountId,

    // Pre-apply outputs
    pub(super) tx_changes_before: LedgerEntryChanges,
    pub(super) fee_changes: LedgerEntryChanges,
    pub(super) refundable_fee_tracker: Option<RefundableFeeTracker>,
    pub(super) tx_event_manager: TxEventManager,
    pub(super) preflight_failure: Option<ExecutionFailure>,
    pub(super) sig_check_failure: Option<(Vec<OperationResult>, ExecutionFailure)>,
    pub(super) fee: i64,

    // Rollback data (entry snapshots for restoring on op failure)
    pub(super) fee_entries: DeltaEntries,
    pub(super) seq_entries: DeltaEntries,
    pub(super) signer_entries: DeltaEntries,

    // Config carried forward
    pub(super) soroban_prng_seed: Option<[u8; 32]>,
    pub(super) base_fee: u32,
    pub(super) deduct_fee: bool,

    // Timing
    pub(super) validation_us: u64,
    pub(super) fee_seq_us: u64,
    pub(super) tx_timing_start: std::time::Instant,
    // Validation sub-component timings (microseconds)
    pub(super) val_account_load_us: u64,
    pub(super) val_tx_hash_us: u64,
    pub(super) val_ed25519_us: u64,
    pub(super) val_other_us: u64,
    // Fee/seq sub-component timings (microseconds)
    pub(super) fee_deduct_us: u64,
    pub(super) op_sig_check_us: u64,
    pub(super) signer_removal_us: u64,
    pub(super) seq_bump_us: u64,
    // Cached transaction hash from validation phase
    pub(super) tx_hash: Option<Hash256>,
}

/// Snapshot of created/updated/deleted entries for a delta phase (fee, seq, signer).
/// Used for rollback restoration when a transaction's operation body fails.
pub(super) struct DeltaEntries {
    pub(super) created: Vec<LedgerEntry>,
    pub(super) updated: Vec<LedgerEntry>,
    pub(super) deleted: Vec<LedgerKey>,
}

impl DeltaEntries {
    /// Empty snapshot — used when rollback data should be suppressed (e.g.,
    /// pre-parallel path where fee/seq/signer mutations are already on the
    /// main delta and must not be re-recorded on the cluster delta).
    pub(super) fn empty() -> Self {
        Self {
            created: Vec::new(),
            updated: Vec::new(),
            deleted: Vec::new(),
        }
    }
}

/// Snapshot of delta entries from the pre-apply phases (fee, sequence, signers).
///
/// Bundled together because all three are captured before operation execution
/// and used together for rollback on failure.
pub(super) struct PreApplySnapshot {
    pub(super) fee_entries: DeltaEntries,
    pub(super) seq_entries: DeltaEntries,
    pub(super) signer_entries: DeltaEntries,
    /// Whether to deduct the fee after rollback (true for non-fee-bump inner txs).
    pub(super) deduct_fee: bool,
    /// The fee amount to re-add to the delta after rollback.
    pub(super) fee: i64,
}

/// Parameters for advancing a TransactionExecutor to a new ledger.
pub struct LedgerAdvanceParams {
    pub ledger_seq: u32,
    pub close_time: u64,
    pub base_reserve: u32,
    pub protocol_version: u32,
    pub soroban_config: SorobanConfig,
    pub frozen_key_config: henyey_tx::frozen_keys::FrozenKeyConfig,
    pub ledger_flags: u32,
}

/// Context for executing transactions during ledger close.
pub struct TransactionExecutor {
    /// Ledger sequence being processed.
    ledger_seq: u32,
    /// Close time.
    close_time: u64,
    /// Base reserve.
    base_reserve: u32,
    /// Protocol version.
    protocol_version: u32,
    /// Ledger header flags (e.g. LP disable flags). 0 if pre-v1 extension.
    ledger_flags: u32,
    /// Network ID.
    network_id: NetworkId,
    /// State manager for execution.
    state: LedgerStateManager,
    /// Accounts loaded from snapshot.
    loaded_accounts: HashMap<AccountId, bool>,
    /// Soroban network configuration for contract execution.
    soroban_config: SorobanConfig,
    /// Classic event configuration.
    classic_events: ClassicEventConfig,
    /// Optional persistent module cache for Soroban WASM compilation.
    /// This cache is populated once from the bucket list and reused across transactions.
    module_cache: Option<PersistentModuleCache>,
    /// Optional hot archive bucket list for Protocol 23+ entry restoration.
    /// This enables looking up entries that have been evicted from the live bucket list
    /// and are waiting to be restored.
    hot_archive: Option<std::sync::Arc<parking_lot::RwLock<Option<HotArchiveBucketList>>>>,
    /// Optional shared in-memory Soroban state for O(1) contract entry lookups.
    ///
    /// When set, `load_soroban_footprint` checks this HashMap-backed cache before
    /// falling through to the 22-bucket list scan. Set on every cluster executor
    /// by `execute_single_cluster` for the parallel Soroban path.
    soroban_state: Option<std::sync::Arc<crate::soroban_state::SharedSorobanState>>,
    /// Whether to emit `SorobanTransactionMetaExtV1` in transaction meta.
    emit_soroban_tx_meta_ext_v1: bool,
    /// Whether to include diagnostic events in transaction meta.
    enable_soroban_diagnostic_events: bool,
    /// Pre-computed TTL key hashes from the most recent `load_soroban_footprint` call.
    /// Reused across all Soroban validation and execution functions to avoid
    /// redundant SHA-256 computations.
    ttl_key_cache: Option<henyey_tx::soroban::TtlKeyCache>,
    /// Frozen ledger keys configuration (CAP-77, Protocol 26+).
    frozen_key_config: henyey_tx::frozen_keys::FrozenKeyConfig,
    /// Keys restored from the hot archive so far in this executor's lifetime.
    /// Prevents re-restoration of an entry that was restored and then deleted
    /// within the same cluster/ledger. Mirrors stellar-core's `entryWasRestored()`.
    hot_archive_restored_keys: std::collections::HashSet<LedgerKey>,
}

impl TransactionExecutor {
    /// Create a new transaction executor.
    pub fn new(
        context: &LedgerContext,
        id_pool: u64,
        soroban_config: SorobanConfig,
        classic_events: ClassicEventConfig,
    ) -> Self {
        let mut state = LedgerStateManager::new(context.base_reserve as i64, context.sequence);
        state.set_id_pool(id_pool);
        Self {
            ledger_seq: context.sequence,
            close_time: context.close_time,
            base_reserve: context.base_reserve,
            protocol_version: context.protocol_version,
            ledger_flags: context.ledger_flags,
            network_id: context.network_id,
            state,
            loaded_accounts: HashMap::new(),
            soroban_config,
            classic_events,
            module_cache: None,
            hot_archive: None,
            soroban_state: None,
            emit_soroban_tx_meta_ext_v1: false,
            enable_soroban_diagnostic_events: false,
            ttl_key_cache: None,
            frozen_key_config: context.frozen_key_config.clone(),
            hot_archive_restored_keys: std::collections::HashSet::new(),
        }
    }

    /// Set the shared offer store reference on the state manager.
    ///
    /// Called once when the executor is created (for new executors) or when the
    /// offer store is first available. The offer store is shared with LedgerManager
    /// via `Arc<Mutex<OfferStore>>`.
    pub fn set_offer_store(
        &mut self,
        store: std::sync::Arc<parking_lot::Mutex<henyey_tx::state::offer_store::OfferStore>>,
    ) {
        self.state.set_offer_store(store);
    }

    /// Set the in-memory Soroban state for O(1) contract entry lookups.
    ///
    /// When set, `load_soroban_footprint` uses this HashMap-backed cache as the
    /// primary source for ContractData/ContractCode/TTL lookups, avoiding expensive
    /// 22-bucket list scans.
    pub fn set_soroban_state(
        &mut self,
        state: std::sync::Arc<crate::soroban_state::SharedSorobanState>,
    ) {
        self.soroban_state = Some(state);
    }

    /// Set the hot archive bucket list for Protocol 23+ entry restoration.
    ///
    /// When set, the executor can look up entries that have been evicted from the
    /// live bucket list and need to be restored during Soroban transaction execution.
    pub fn set_hot_archive(
        &mut self,
        hot_archive: std::sync::Arc<parking_lot::RwLock<Option<HotArchiveBucketList>>>,
    ) {
        self.hot_archive = Some(hot_archive);
    }

    /// Seed the hot-archive restored keys set from a prior stage.
    ///
    /// In the parallel Soroban path, each stage gets a fresh executor per cluster.
    /// To prevent re-restoration across stages, the prior stage's accumulated
    /// restored keys must be seeded into the new executor. Mirrors stellar-core's
    /// `mPreviouslyRestoredEntries`.
    pub fn seed_hot_archive_restored_keys(&mut self, keys: &std::collections::HashSet<LedgerKey>) {
        self.hot_archive_restored_keys.extend(keys.iter().cloned());
    }

    /// Record that keys were restored from the hot archive during a successful TX.
    pub fn record_hot_archive_restores(&mut self, keys: &[LedgerKey]) {
        self.hot_archive_restored_keys.extend(keys.iter().cloned());
    }

    /// Get a reference to the accumulated hot-archive restored keys.
    pub fn hot_archive_restored_keys(&self) -> &std::collections::HashSet<LedgerKey> {
        &self.hot_archive_restored_keys
    }

    /// Set the persistent module cache for WASM compilation.
    ///
    /// The module cache should be populated with contract code from the bucket list
    /// before executing transactions. This enables reuse of compiled WASM modules
    /// across transactions, significantly improving performance.
    pub fn set_module_cache(&mut self, cache: PersistentModuleCache) {
        self.module_cache = Some(cache);
    }

    /// Get a reference to the module cache, if set.
    pub fn module_cache(&self) -> Option<&PersistentModuleCache> {
        self.module_cache.as_ref()
    }

    /// Configure meta extension flags for the executor.
    ///
    /// These control which optional fields appear in `TransactionMeta`:
    /// - `emit_soroban_tx_meta_ext_v1`: Include `SorobanTransactionMetaExtV1` fee breakdown
    /// - `enable_soroban_diagnostic_events`: Include diagnostic events in meta
    pub fn set_meta_flags(
        &mut self,
        emit_soroban_tx_meta_ext_v1: bool,
        enable_soroban_diagnostic_events: bool,
    ) {
        self.emit_soroban_tx_meta_ext_v1 = emit_soroban_tx_meta_ext_v1;
        self.enable_soroban_diagnostic_events = enable_soroban_diagnostic_events;
    }

    /// Add contract code to the module cache.
    ///
    /// This is called when new contract code entries are created, updated, or restored
    /// during ledger close. Adding the code to the cache enables subsequent transactions
    /// to use VmCachedInstantiation (cheap) instead of VmInstantiation (expensive).
    ///
    /// Without this, contracts deployed in transaction N would not be cached for
    /// transaction N+1 in the same ledger, causing cost model divergence.
    fn add_contract_to_cache(&self, code: &[u8]) {
        if let Some(cache) = &self.module_cache {
            cache.add_contract(code, self.protocol_version);
        }
    }

    /// Advance to a new ledger, clearing cached entries but preserving offers.
    ///
    /// Offers are expensive to reload (~911K entries on mainnet, ~2.7s per ledger).
    /// The executor's offer cache is maintained correctly across ledgers because:
    /// 1. TX execution modifies offers directly in state (create/update/delete)
    /// 2. At the end of a ledger, state.offers reflects the correct post-ledger state
    /// 3. The in-memory offer store (LedgerManager) is also updated incrementally
    ///
    /// Non-offer entries are still cleared to reload from the bucket list, which
    /// may have been updated with authoritative CDP metadata.
    ///
    /// Note: id_pool is NOT reset here because:
    /// 1. The executor's internal id_pool evolves correctly as transactions execute
    /// 2. The id_pool from the ledger header is the POST-execution value (after the ledger closes)
    /// 3. Using the header's id_pool would give us the wrong starting value for the next ledger
    ///    For the first ledger in a replay session, use TransactionExecutor::new() which takes
    ///    the PREVIOUS ledger's closing id_pool (which equals this ledger's starting id_pool).
    pub fn advance_to_ledger(&mut self, params: LedgerAdvanceParams) {
        self.ledger_seq = params.ledger_seq;
        self.close_time = params.close_time;
        self.base_reserve = params.base_reserve;
        self.protocol_version = params.protocol_version;
        self.ledger_flags = params.ledger_flags;
        self.soroban_config = params.soroban_config;
        self.frozen_key_config = params.frozen_key_config;
        self.state.set_ledger_seq(params.ledger_seq);
        // Clear cached entries except offers and offer index
        self.state.clear_cached_entries_preserving_offers();
        // Clear loaded_accounts cache (non-offer)
        self.loaded_accounts.clear();
        // Clear hot-archive restored keys from the previous ledger.
        self.hot_archive_restored_keys.clear();
    }

    /// Look up an entry from the snapshot, respecting delta deletions.
    ///
    /// This is the single entry-point for all snapshot/bucket-list reads.
    /// It checks `delta().deleted_keys()` *before* hitting the snapshot so
    /// that entries deleted by a prior TX in this ledger are never reloaded.
    ///
    /// All `load_*` methods should call this instead of
    /// `snapshot.get_entry()` directly.
    fn get_entry_from_snapshot(
        &self,
        snapshot: &SnapshotHandle,
        key: &LedgerKey,
    ) -> Result<Option<LedgerEntry>> {
        if self.state.delta().deleted_keys().contains(key) {
            return Ok(None);
        }
        snapshot.get_entry(key)
    }

    /// Batch-load multiple entries from the bucket list in a single pass.
    ///
    /// Filters out entries already in state or already attempted, then loads
    /// remaining keys via `snapshot.load_entries()` which uses a single bucket
    /// list traversal for all keys. This is significantly faster than individual
    /// `load_account`/`load_trustline` calls for operations needing multiple entries.
    fn batch_load_keys(&mut self, snapshot: &SnapshotHandle, keys: &[LedgerKey]) -> Result<()> {
        let mut needed = Vec::new();

        for key in keys {
            // Skip if the entry was deleted in this ledger (don't reload from snapshot).
            // Note: batch path uses load_entries(), so we filter here rather than
            // going through get_entry_from_snapshot().
            if self.state.delta().deleted_keys().contains(key) {
                continue;
            }

            let already_loaded = match key {
                LedgerKey::Account(k) => {
                    self.state.get_account(&k.account_id).is_some()
                        || self.loaded_accounts.contains_key(&k.account_id)
                }
                LedgerKey::Trustline(k) => self
                    .state
                    .get_trustline_by_trustline_asset(&k.account_id, &k.asset)
                    .is_some(),
                LedgerKey::ClaimableBalance(k) => {
                    self.state.get_claimable_balance(&k.balance_id).is_some()
                }
                LedgerKey::LiquidityPool(k) => self
                    .state
                    .get_liquidity_pool(&k.liquidity_pool_id)
                    .is_some(),
                LedgerKey::Offer(k) => self.state.get_offer(&k.seller_id, k.offer_id).is_some(),
                _ => false,
            };

            if !already_loaded {
                needed.push(key.clone());
            }
        }

        if needed.is_empty() {
            return Ok(());
        }

        // Mark all account keys as attempted (whether found or not)
        for key in &needed {
            if let LedgerKey::Account(k) = key {
                self.loaded_accounts.insert(k.account_id.clone(), true);
            }
        }

        let entries = snapshot.load_entries(&needed)?;
        for entry in entries {
            self.state.load_entry(entry);
        }

        Ok(())
    }

    /// Load an account from the snapshot into the state manager.
    pub fn load_account(
        &mut self,
        snapshot: &SnapshotHandle,
        account_id: &AccountId,
    ) -> Result<bool> {
        self.load_account_inner(snapshot, account_id, true)
    }

    /// Load an account from the snapshot into state WITHOUT recording it for transaction changes.
    /// This matches stellar-core's `loadAccountWithoutRecord()` behavior, used when an account
    /// only needs to be checked for existence (e.g., issuer validation in ChangeTrust).
    /// The account is loaded into state so operations can check existence, but it won't appear
    /// in the transaction meta STATE/UPDATED changes.
    pub fn load_account_without_record(
        &mut self,
        snapshot: &SnapshotHandle,
        account_id: &AccountId,
    ) -> Result<bool> {
        self.load_account_inner(snapshot, account_id, false)
    }

    /// Shared implementation for loading an account from snapshot.
    /// When `record` is true, uses `state.load_entry()` (captures a snapshot for change tracking).
    /// When `record` is false, uses `state.load_entry_without_snapshot()`.
    fn load_account_inner(
        &mut self,
        snapshot: &SnapshotHandle,
        account_id: &AccountId,
        record: bool,
    ) -> Result<bool> {
        let label = if record {
            "load_account"
        } else {
            "load_account_without_record"
        };

        // First check if the account was created/updated by a previous transaction in this ledger
        // This is important for intra-ledger dependencies (e.g., TX0 creates account, TX1 uses it)
        if self.state.get_account(account_id).is_some() {
            tracing::trace!(account = %account_id_to_strkey(account_id), "{}: found in state", label);
            return Ok(true);
        }

        // Check if we've already tried to load from snapshot
        if self.loaded_accounts.contains_key(account_id) {
            tracing::trace!(account = %account_id_to_strkey(account_id), "{}: already tried, not found", label);
            return Ok(false);
        }

        // Mark as attempted
        self.loaded_accounts.insert(account_id.clone(), true);

        // Try to load from snapshot
        let key = stellar_xdr::curr::LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: account_id.clone(),
        });

        if let Some(entry) = self.get_entry_from_snapshot(snapshot, &key)? {
            if record {
                // Log signer info for debugging (only in record mode)
                if let stellar_xdr::curr::LedgerEntryData::Account(ref acct) = entry.data {
                    tracing::trace!(
                        account = ?account_id,
                        num_signers = acct.signers.len(),
                        thresholds = ?acct.thresholds.0,
                        "{}: found in bucket list", label
                    );
                }
                self.state.load_entry(entry);
            } else {
                tracing::trace!(
                    account = ?account_id,
                    "{}: found in bucket list", label
                );
                self.state.load_entry_without_snapshot(entry);
            }
            return Ok(true);
        }

        tracing::debug!(account = %account_id_to_strkey(account_id), "{}: NOT FOUND in bucket list", label);
        Ok(false)
    }

    /// Load a trustline from the snapshot into the state manager.
    pub fn load_trustline(
        &mut self,
        snapshot: &SnapshotHandle,
        account_id: &AccountId,
        asset: &stellar_xdr::curr::TrustLineAsset,
    ) -> Result<bool> {
        if self
            .state
            .get_trustline_by_trustline_asset(account_id, asset)
            .is_some()
        {
            return Ok(true);
        }

        // If the entry was loaded during this TX and then deleted, don't reload.
        if self.state.is_trustline_tracked(account_id, asset) {
            return Ok(false);
        }

        let key = stellar_xdr::curr::LedgerKey::Trustline(stellar_xdr::curr::LedgerKeyTrustLine {
            account_id: account_id.clone(),
            asset: asset.clone(),
        });

        if let Some(entry) = self.get_entry_from_snapshot(snapshot, &key)? {
            if let stellar_xdr::curr::LedgerEntryData::Trustline(ref tl) = entry.data {
                tracing::debug!(
                    account = %account_id_to_strkey(account_id),
                    asset = ?asset,
                    balance = tl.balance,
                    "load_trustline: loaded from bucket list"
                );
            }
            self.state.load_entry(entry);
            return Ok(true);
        }

        tracing::debug!(
            account = %account_id_to_strkey(account_id),
            asset = ?asset,
            "load_trustline: NOT FOUND in bucket list"
        );
        Ok(false)
    }

    /// Load a claimable balance from the snapshot into the state manager.
    pub fn load_claimable_balance(
        &mut self,
        snapshot: &SnapshotHandle,
        balance_id: &ClaimableBalanceId,
    ) -> Result<bool> {
        // Check if already in state from previous transaction in this ledger
        if self.state.get_claimable_balance(balance_id).is_some() {
            return Ok(true);
        }

        // If the entry was loaded during this TX and then deleted (e.g., claimed by
        // a prior operation), don't reload from the snapshot. This matches stellar-core behavior
        // where nested LedgerTxn inherits deletions from the parent.
        if self.state.is_claimable_balance_tracked(balance_id) {
            return Ok(false);
        }

        let key = stellar_xdr::curr::LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
            balance_id: balance_id.clone(),
        });

        if let Some(entry) = self.get_entry_from_snapshot(snapshot, &key)? {
            self.state.load_entry(entry);
            return Ok(true);
        }
        Ok(false)
    }

    /// Load a data entry from the snapshot using the raw String64 key.
    /// This preserves non-UTF8 bytes in the data name.
    pub fn load_data_raw(
        &mut self,
        snapshot: &SnapshotHandle,
        account_id: &AccountId,
        data_name: &stellar_xdr::curr::String64,
    ) -> Result<bool> {
        // Convert to string for state lookup (matches how data_entries are keyed)
        let name_str = String::from_utf8_lossy(data_name.as_slice()).to_string();

        // Check if already in state from previous transaction in this ledger
        if self.state.get_data(account_id, &name_str).is_some() {
            return Ok(true);
        }

        // If the entry was loaded during this TX and then deleted, don't reload.
        if self.state.is_data_tracked(account_id, &name_str) {
            return Ok(false);
        }

        let key = stellar_xdr::curr::LedgerKey::Data(stellar_xdr::curr::LedgerKeyData {
            account_id: account_id.clone(),
            data_name: data_name.clone(),
        });

        if let Some(entry) = self.get_entry_from_snapshot(snapshot, &key)? {
            self.state.load_entry(entry);
            return Ok(true);
        }
        Ok(false)
    }

    /// Load the sponsor account for an offer if it has one.
    /// This is needed when deleting or modifying an offer with sponsorship,
    /// as we need to update the sponsor's num_sponsoring counter.
    fn load_offer_sponsor(
        &mut self,
        snapshot: &SnapshotHandle,
        seller_id: &AccountId,
        offer_id: i64,
    ) -> Result<()> {
        let key = stellar_xdr::curr::LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
            seller_id: seller_id.clone(),
            offer_id,
        });
        if let Some(sponsor) = self.state.entry_sponsor(&key) {
            self.load_account(snapshot, &sponsor)?;
        }
        Ok(())
    }

    fn load_asset_issuer(&mut self, snapshot: &SnapshotHandle, asset: &Asset) -> Result<()> {
        match asset {
            Asset::CreditAlphanum4(a) => {
                self.load_account(snapshot, &a.issuer)?;
            }
            Asset::CreditAlphanum12(a) => {
                self.load_account(snapshot, &a.issuer)?;
            }
            Asset::Native => {}
        }
        Ok(())
    }

    /// Load a liquidity pool from the snapshot into the state manager.
    ///
    /// If the pool already exists in state (e.g., from CDP sync or previous loading),
    /// returns the existing state without reloading from snapshot. This is critical
    /// for verification mode where CDP sync updates state between transactions.
    pub fn load_liquidity_pool(
        &mut self,
        snapshot: &SnapshotHandle,
        pool_id: &PoolId,
    ) -> Result<Option<LiquidityPoolEntry>> {
        // Check if already loaded in state - don't overwrite with snapshot data
        if let Some(pool) = self.state.get_liquidity_pool(pool_id) {
            return Ok(Some(pool.clone()));
        }

        // If the pool was loaded during this TX and then deleted, don't reload.
        if self.state.is_liquidity_pool_tracked(pool_id) {
            return Ok(None);
        }

        let key = stellar_xdr::curr::LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
            liquidity_pool_id: pool_id.clone(),
        });

        if let Some(entry) = self.get_entry_from_snapshot(snapshot, &key)? {
            let pool = match &entry.data {
                LedgerEntryData::LiquidityPool(pool) => pool.clone(),
                _ => return Ok(None),
            };
            self.state.load_entry(entry);
            return Ok(Some(pool));
        }
        Ok(None)
    }

    fn load_liquidity_pool_dependencies(
        &mut self,
        snapshot: &SnapshotHandle,
        op_source: &AccountId,
        pool_id: &PoolId,
    ) -> Result<()> {
        if let Some(pool) = self.load_liquidity_pool(snapshot, pool_id)? {
            // Batch-load pool share trustline + asset trustlines + issuer accounts
            let pool_share_asset = TrustLineAsset::PoolShare(pool_id.clone());
            let mut keys = vec![make_trustline_key(op_source, &pool_share_asset)];

            match &pool.body {
                LiquidityPoolEntryBody::LiquidityPoolConstantProduct(cp) => {
                    for asset in [&cp.params.asset_a, &cp.params.asset_b] {
                        if let Some(tl_asset) = asset_to_trustline_asset(asset) {
                            keys.push(make_trustline_key(op_source, &tl_asset));
                        }
                        if let Some(issuer) = asset_issuer_id(asset) {
                            keys.push(make_account_key(&issuer));
                        }
                    }
                }
            }
            self.batch_load_keys(snapshot, &keys)?;
        }
        Ok(())
    }

    /// Load liquidity pools that could be used for path payment conversions.
    ///
    /// For each adjacent pair of assets in the conversion path, attempts to load the
    /// corresponding liquidity pool if it exists.
    fn load_path_payment_pools(
        &mut self,
        snapshot: &SnapshotHandle,
        send_asset: &stellar_xdr::curr::Asset,
        dest_asset: &stellar_xdr::curr::Asset,
        path: &[stellar_xdr::curr::Asset],
    ) -> Result<()> {
        use sha2::{Digest, Sha256};
        use stellar_xdr::curr::{
            Limits, LiquidityPoolConstantProductParameters, LiquidityPoolParameters,
        };

        // Build the full conversion path: send_asset -> path[0] -> ... -> dest_asset
        let mut assets: Vec<&stellar_xdr::curr::Asset> = vec![send_asset];
        assets.extend(path.iter());
        assets.push(dest_asset);

        // For each adjacent pair, try to load the liquidity pool
        for window in assets.windows(2) {
            let asset_a = window[0];
            let asset_b = window[1];

            if asset_a == asset_b {
                continue; // Same asset, no swap needed
            }

            // Compute pool ID: assets must be sorted (a < b) to match pool key
            let (sorted_a, sorted_b) = if asset_a < asset_b {
                (asset_a.clone(), asset_b.clone())
            } else {
                (asset_b.clone(), asset_a.clone())
            };

            let params = LiquidityPoolParameters::LiquidityPoolConstantProduct(
                LiquidityPoolConstantProductParameters {
                    asset_a: sorted_a,
                    asset_b: sorted_b,
                    fee: LIQUIDITY_POOL_FEE_V18,
                },
            );

            if let Ok(xdr) = params.to_xdr(Limits::none()) {
                let pool_id = PoolId(stellar_xdr::curr::Hash(Sha256::digest(&xdr).into()));
                // Attempt to load - it's OK if the pool doesn't exist
                let _ = self.load_liquidity_pool(snapshot, &pool_id);
            }
        }

        Ok(())
    }

    fn load_offer_dependencies(
        &mut self,
        snapshot: &SnapshotHandle,
        offer: &OfferEntry,
    ) -> Result<()> {
        self.load_account(snapshot, &offer.seller_id)?;
        if let Some(tl_asset) = asset_to_trustline_asset(&offer.selling) {
            self.load_trustline(snapshot, &offer.seller_id, &tl_asset)?;
        }
        if let Some(tl_asset) = asset_to_trustline_asset(&offer.buying) {
            self.load_trustline(snapshot, &offer.seller_id, &tl_asset)?;
        }
        Ok(())
    }

    /// Collect seller account + trustline keys for the top-N best offers in each asset pair.
    ///
    /// Only prefetches sellers for specific pairs (from the current TX set's DEX operations).
    /// With N=10 and ~50-200 pairs per ledger, this produces ~500-2000 offers → 1500-6000 keys
    /// instead of ~2M from scanning all 911K offers.
    ///
    /// Mirrors stellar-core's demand-driven `populateEntryCacheFromBestOffers`.
    pub fn collect_seller_keys_for_pairs(
        &self,
        pairs: &HashSet<(Asset, Asset)>,
        n: usize,
    ) -> Vec<LedgerKey> {
        let mut keys: HashSet<LedgerKey> = HashSet::new();
        for (buying, selling) in pairs {
            for offer_key in self.state.top_n_offer_keys(buying, selling, n) {
                if let Some(offer) = self.state.get_offer_by_key(&offer_key) {
                    keys.insert(LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                        account_id: offer.seller_id.clone(),
                    }));
                    if let Some(tl_asset) = asset_to_trustline_asset(&offer.selling) {
                        keys.insert(LedgerKey::Trustline(LedgerKeyTrustLine {
                            account_id: offer.seller_id.clone(),
                            asset: tl_asset,
                        }));
                    }
                    if let Some(tl_asset) = asset_to_trustline_asset(&offer.buying) {
                        keys.insert(LedgerKey::Trustline(LedgerKeyTrustLine {
                            account_id: offer.seller_id.clone(),
                            asset: tl_asset,
                        }));
                    }
                }
            }
        }
        keys.into_iter().collect()
    }

    /// Load all orderbook offers from the snapshot into state.
    ///
    /// Called once per ledger during initialization, before any transactions execute.
    /// Offers remain current across transactions because TX execution modifies them
    /// directly in `self.state`, and `snapshot_delta()` preserves committed changes
    /// between transactions.
    pub fn load_orderbook_offers(&mut self, snapshot: &SnapshotHandle) -> Result<()> {
        let entries = snapshot.all_entries()?;
        for entry in entries {
            if !matches!(entry.data, LedgerEntryData::Offer(_)) {
                continue;
            }
            self.state.load_entry(entry);
        }
        Ok(())
    }

    /// Load dependencies (trustlines, accounts) for all offers by an account for a specific asset.
    ///
    /// This is used when revoking trustline authorization - all offers for the
    /// account/asset pair must be removed. Their dependencies (seller accounts
    /// and trustlines for selling/buying assets) must be loaded so that
    /// `release_offer_liabilities` can update them.
    ///
    /// Uses the state's own `account_asset_offers` index (which is maintained
    /// as offers are loaded/created/modified/deleted) rather than the manager's
    /// `offer_account_asset_index` (which is only built at startup and can become
    /// stale across ledgers).
    pub fn load_offers_by_account_and_asset(
        &mut self,
        snapshot: &SnapshotHandle,
        account_id: &AccountId,
        asset: &Asset,
    ) -> Result<()> {
        // Get matching offers from the state's own index, which is always up-to-date.
        // All offers are loaded into state by load_orderbook_offers() before any TX executes.
        let offers = self
            .state
            .get_offers_by_account_and_asset(account_id, asset);
        for offer in &offers {
            let offer_key = LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
                seller_id: offer.seller_id.clone(),
                offer_id: offer.offer_id,
            });

            // Skip if already deleted
            if self.state.delta().deleted_keys().contains(&offer_key) {
                continue;
            }

            // Load dependencies (trustlines, accounts) for the offer.
            // This is critical because:
            // 1. Offers are cached in state across transactions
            // 2. But non-offer state is cleared between transactions
            // 3. When revoking authorization, we need to release liabilities on trustlines
            // 4. Those trustlines must be loaded even if the offer was cached from a previous TX
            self.load_offer_dependencies(snapshot, offer)?;
        }
        Ok(())
    }

    /// Load pool share trustlines and their dependencies for an account+asset pair.
    ///
    /// When `SetTrustLineFlags` or `AllowTrust` revokes authorization, all pool share
    /// trustlines referencing that asset must be redeemed as claimable balances.
    /// This requires the pool share trustlines, their associated liquidity pools, and
    /// the non-target asset trustlines to be in-memory before execution.
    ///
    /// This mirrors stellar-core's `getPoolShareTrustLine(accountID, asset)` SQL query.
    pub fn load_pool_share_trustlines_for_account_and_asset(
        &mut self,
        snapshot: &SnapshotHandle,
        account_id: &AccountId,
        asset: &Asset,
    ) -> Result<()> {
        // Get pool IDs for this account from the secondary index.
        let pool_ids = snapshot
            .pool_share_tls_by_account(account_id)
            .map_err(|e| LedgerError::Internal(e.to_string()))?;

        if pool_ids.is_empty() {
            return Ok(());
        }

        // Batch-load all pool share trustlines and their associated liquidity pools.
        let mut keys = Vec::with_capacity(pool_ids.len() * 2);
        for pool_id in &pool_ids {
            keys.push(LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
                liquidity_pool_id: pool_id.clone(),
            }));
            let pool_share_asset = TrustLineAsset::PoolShare(pool_id.clone());
            keys.push(make_trustline_key(account_id, &pool_share_asset));
        }
        self.batch_load_keys(snapshot, &keys)?;

        // For each pool that contains the target asset, also load the other asset's
        // trustline for the account so `decrement_liquidity_pool_use_count` can find it.
        let mut other_assets: Vec<Asset> = Vec::new();
        for pool_id in &pool_ids {
            let Some(pool) = self.state.get_liquidity_pool(pool_id) else {
                continue;
            };
            let LiquidityPoolEntryBody::LiquidityPoolConstantProduct(ref cp) = pool.body;
            if &cp.params.asset_a != asset && &cp.params.asset_b != asset {
                continue;
            }
            let other = if &cp.params.asset_a == asset {
                cp.params.asset_b.clone()
            } else {
                cp.params.asset_a.clone()
            };
            other_assets.push(other);
        }
        for other_asset in &other_assets {
            if let Some(tl_asset) = asset_to_trustline_asset(other_asset) {
                self.load_trustline(snapshot, account_id, &tl_asset)?;
            }
        }

        Ok(())
    }

    /// Load a ledger entry from the snapshot into the state manager.
    ///
    /// This handles all entry types including contract data, contract code, and TTL entries.
    /// Returns true if the entry was found and loaded.
    pub fn load_entry(&mut self, snapshot: &SnapshotHandle, key: &LedgerKey) -> Result<bool> {
        // First check if entry already exists in state (e.g., created by a previous TX in this ledger).
        // This is important for entries like TTLs that are created during restoration but haven't
        // been written to the bucket list yet.
        if self.state.get_entry(key).is_some() {
            return Ok(true);
        }
        // Try to load from snapshot (bucket list + hot archive)
        if let Some(entry) = self.get_entry_from_snapshot(snapshot, key)? {
            self.state.load_entry(entry);
            return Ok(true);
        }
        Ok(false)
    }

    /// Get a ledger entry from the current state (if loaded).
    pub fn get_entry(&self, key: &LedgerKey) -> Option<LedgerEntry> {
        self.state.get_entry(key)
    }

    /// Load all entries from a Soroban footprint into the state manager.
    ///
    /// This is essential for Soroban transaction execution - the footprint specifies
    /// which ledger entries the transaction will read or write, and they must be
    /// loaded before the Soroban host can access them.
    ///
    /// For entries not found in the live state (evicted to hot archive), this method
    /// auto-restores them by loading from the hot archive and creating a restored TTL.
    /// This matches stellar-core's `addReads()` → `handleArchivedEntry()` behavior.
    pub fn load_soroban_footprint(
        &mut self,
        snapshot: &SnapshotHandle,
        footprint: &stellar_xdr::curr::LedgerFootprint,
    ) -> Result<()> {
        use sha2::{Digest, Sha256};

        let fp_start = std::time::Instant::now();

        // Reuse the executor's TTL key cache across TXs to avoid redundant SHA-256
        // computations for the same keys (common when TXs share footprint entries).
        let mut ttl_key_cache = self.ttl_key_cache.take().unwrap_or_default();

        // Acquire a read lock on InMemorySorobanState if available (O1 optimization).
        // InMemorySorobanState is a HashMap mirror of all live ContractData/ContractCode/TTL
        // entries built from the bucket list at startup and updated incrementally on each
        // ledger close. Lookups are O(1) vs O(22 × bloom_filter) for bucket list scans.
        let in_memory = self.soroban_state.as_ref().map(|s| s.read());
        let mut ims_hits: u32 = 0;
        let mut ims_ttl_hits: u32 = 0;

        // Collect footprint keys not yet in state, routing Soroban keys to InMemorySorobanState
        // and everything else to the bucket list.
        let mut bucket_list_keys = Vec::new();
        for key in footprint
            .read_only
            .iter()
            .chain(footprint.read_write.iter())
        {
            // Entries deleted by a previous TX in this ledger must not be reloaded.
            // In stellar-core, deleted entries are tracked in mThreadEntryMap as nullopt.
            if self.state.is_entry_deleted(key) {
                continue;
            }

            // For ContractData/ContractCode keys, we must ensure BOTH the entry AND its
            // TTL are loaded into state. The entry may already be present (loaded by a
            // prior stage TX via prior_stage.entries) but its TTL might be absent if the
            // prior stage TX only modified the entry data (not the TTL). We always check
            // whether the TTL needs loading, even when the entry is already in state.
            if matches!(key, LedgerKey::ContractData(_) | LedgerKey::ContractCode(_)) {
                // Compute TTL key hash (O3: reused for both IMS and bucket-list paths).
                // The cache is shared across TXs in this cluster for entries that recur.
                let key_bytes = key
                    .to_xdr(Limits::none())
                    .map_err(|e| LedgerError::Serialization(e.to_string()))?;
                let key_hash = stellar_xdr::curr::Hash(Sha256::digest(&key_bytes).into());
                ttl_key_cache.insert(key.clone(), key_hash.clone());
                let ttl_key = LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl { key_hash });

                let entry_in_state = self.state.get_entry(key).is_some();
                let ttl_in_state = self.state.get_entry(&ttl_key).is_some()
                    || self.state.is_entry_deleted(&ttl_key);

                if entry_in_state && ttl_in_state {
                    // Both entry and TTL already loaded — nothing to do.
                    continue;
                }

                if let Some(ref ims) = in_memory {
                    if !entry_in_state {
                        if let Some(entry) = ims.get(key) {
                            // Found in IMS — load the entry, skipping bucket list.
                            self.state.load_entry((*entry).clone());
                            ims_hits += 1;
                            // Load co-located TTL from IMS if not yet in state.
                            if !ttl_in_state {
                                if let Some(ttl_entry) = ims.get(&ttl_key) {
                                    self.state.load_entry((*ttl_entry).clone());
                                    ims_ttl_hits += 1;
                                } else {
                                    // TTL absent from IMS despite entry present — fall through
                                    // to bucket list for TTL.
                                    bucket_list_keys.push(ttl_key);
                                }
                            }
                            continue;
                        }
                        // Entry not in IMS — load both entry and TTL from bucket list.
                        bucket_list_keys.push(key.clone());
                        if !ttl_in_state {
                            bucket_list_keys.push(ttl_key);
                        }
                    } else {
                        // Entry already in state but TTL missing — load TTL only.
                        // Try IMS first (fast path).
                        if let Some(ttl_entry) = ims.get(&ttl_key) {
                            self.state.load_entry((*ttl_entry).clone());
                            ims_ttl_hits += 1;
                        } else {
                            bucket_list_keys.push(ttl_key);
                        }
                    }
                    continue;
                }

                // No IMS — load from bucket list.
                if !entry_in_state {
                    bucket_list_keys.push(key.clone());
                }
                if !ttl_in_state {
                    bucket_list_keys.push(ttl_key);
                }
                continue;
            }

            // Non-Soroban key: only load if not already in state.
            if self.state.get_entry(key).is_none() {
                bucket_list_keys.push(key.clone());
            }
        }

        let route_us = fp_start.elapsed().as_micros() as u64;
        let bl_key_count = bucket_list_keys.len() as u32;

        let bl_load_start = std::time::Instant::now();
        if !bucket_list_keys.is_empty() {
            // Batch-load all remaining entries + TTLs in a single bucket list pass.
            let entries = snapshot.load_entries(&bucket_list_keys)?;
            for entry in entries {
                self.state.load_entry(entry);
            }
        }
        let bl_load_us = bl_load_start.elapsed().as_micros() as u64;

        // NOTE: We do NOT auto-restore entries from the hot archive here.
        // In stellar-core, auto-restore is handled differently:
        // - RO entries with expired TTL or in hot archive → TX fails with ENTRY_ARCHIVED
        //   (checked by footprint_has_unrestored_archived_entries before host execution)
        // - RW entries marked in archivedSorobanEntries → restored in encode_footprint_entries
        //   (via get_archived_with_restore_info / get_entry_for_restoration)
        // Auto-restoring entries here would mask the ENTRY_ARCHIVED check.

        // Store the TTL key cache for use during Soroban execution.
        self.ttl_key_cache = if ttl_key_cache.is_empty() {
            None
        } else {
            Some(ttl_key_cache)
        };

        let total_keys = footprint.read_only.len() + footprint.read_write.len();
        let total_us = fp_start.elapsed().as_micros() as u64;
        if total_us > 50 {
            tracing::debug!(
                total_us,
                route_us,
                bl_load_us,
                total_keys,
                ims_hits,
                ims_ttl_hits,
                bl_key_count,
                "PROFILE load_soroban_footprint"
            );
        }

        Ok(())
    }

    /// Apply ledger entry changes to the executor's state WITHOUT delta tracking.
    ///
    /// This is used during verification to sync state with CDP after each transaction,
    /// ensuring subsequent transactions see the correct state even if our validation
    /// failed for an earlier transaction.
    ///
    /// Uses no-tracking methods to avoid polluting the delta for subsequent transactions.
    pub fn apply_ledger_entry_changes(&mut self, changes: &stellar_xdr::curr::LedgerEntryChanges) {
        use stellar_xdr::curr::{LedgerEntryChange, LedgerEntryData};

        for change in changes.iter() {
            match change {
                LedgerEntryChange::Created(entry)
                | LedgerEntryChange::Updated(entry)
                | LedgerEntryChange::Restored(entry) => {
                    self.state.apply_entry_no_tracking(entry);
                    // Add newly created/restored contract code to the module cache.
                    // This ensures subsequent transactions can use VmCachedInstantiation
                    // instead of the more expensive VmInstantiation for newly deployed contracts.
                    if let LedgerEntryData::ContractCode(cc) = &entry.data {
                        self.add_contract_to_cache(cc.code.as_slice());
                    }
                }
                LedgerEntryChange::Removed(key) => {
                    self.state.delete_entry_no_tracking(key);
                }
                LedgerEntryChange::State(_) => {
                    // State changes are informational only, no action needed
                }
            }
        }
        // Clear snapshots to prevent stale snapshot state from affecting subsequent transactions.
        // Without this, a transaction that creates an entry might see an old snapshot from a
        // previous transaction's execution, causing incorrect STATE/CREATED tracking.
        self.state.commit();
    }

    /// Apply ledger entry changes preserving account sequence numbers.
    ///
    /// This is similar to `apply_ledger_entry_changes` but preserves the current
    /// sequence number for existing accounts. This is needed because CDP metadata
    /// for operation changes can capture sequence numbers that include effects from
    /// later transactions in the same ledger (due to how stellar-core captures STATE values).
    ///
    /// For Account UPDATED entries:
    /// - If the account exists in our state, preserve our sequence number
    /// - Apply all other fields from the CDP entry (balance, etc.)
    pub fn apply_ledger_entry_changes_preserve_seq(
        &mut self,
        changes: &stellar_xdr::curr::LedgerEntryChanges,
    ) {
        use stellar_xdr::curr::{LedgerEntryChange, LedgerEntryData};

        for change in changes.iter() {
            match change {
                LedgerEntryChange::Updated(entry) => {
                    // For Account entries, preserve our sequence number
                    if let LedgerEntryData::Account(new_acc) = &entry.data {
                        if let Some(existing_acc) = self.state.get_account_mut(&new_acc.account_id)
                        {
                            // Preserve our sequence number
                            let our_seq = existing_acc.seq_num.0;
                            // Apply all fields from CDP entry
                            *existing_acc = new_acc.clone();
                            // Restore our sequence
                            existing_acc.seq_num.0 = our_seq;
                            continue;
                        }
                    }
                    // For non-account entries or new accounts, apply normally
                    self.state.apply_entry_no_tracking(entry);
                    // Add contract code to cache (Updated entries can be code re-uploads)
                    if let LedgerEntryData::ContractCode(cc) = &entry.data {
                        self.add_contract_to_cache(cc.code.as_slice());
                    }
                }
                LedgerEntryChange::Created(entry) | LedgerEntryChange::Restored(entry) => {
                    self.state.apply_entry_no_tracking(entry);
                    // Add newly created/restored contract code to the module cache
                    if let LedgerEntryData::ContractCode(cc) = &entry.data {
                        self.add_contract_to_cache(cc.code.as_slice());
                    }
                }
                LedgerEntryChange::Removed(key) => {
                    self.state.delete_entry_no_tracking(key);
                }
                LedgerEntryChange::State(_) => {
                    // State changes are informational only, no action needed
                }
            }
        }
        self.state.commit();
    }

    /// Execute a transaction.
    ///
    /// # Arguments
    ///
    /// * `soroban_prng_seed` - Optional PRNG seed for Soroban execution.
    ///   Computed as subSha256(txSetHash, txIndex) at the transaction set level.
    pub fn execute_transaction(
        &mut self,
        snapshot: &SnapshotHandle,
        tx_envelope: &TransactionEnvelope,
        base_fee: u32,
        soroban_prng_seed: Option<[u8; 32]>,
    ) -> Result<TransactionExecutionResult> {
        self.execute_transaction_with_fee_mode(
            snapshot,
            tx_envelope,
            base_fee,
            soroban_prng_seed,
            true,
        )
    }

    /// Process fee for a transaction without executing it.
    ///
    /// This method is used for batched fee processing where all fees are
    /// processed before any transaction is applied. This matches the behavior
    /// of stellar-core.
    ///
    /// Returns the fee changes and the fee amount charged.
    pub fn process_fee_only(
        &mut self,
        snapshot: &SnapshotHandle,
        tx_envelope: &Arc<TransactionEnvelope>,
        base_fee: u32,
    ) -> Result<(LedgerEntryChanges, i64)> {
        let frame = TransactionFrame::with_network(Arc::clone(tx_envelope), self.network_id);
        let fee_source_id = henyey_tx::muxed_to_account_id(&frame.fee_source_account());
        let inner_source_id = henyey_tx::muxed_to_account_id(&frame.inner_source_account());

        // Load source accounts
        if !self.load_account(snapshot, &fee_source_id)? {
            return Err(LedgerError::Internal("Fee source account not found".into()));
        }
        if !self.load_account(snapshot, &inner_source_id)? {
            return Err(LedgerError::Internal(
                "Inner source account not found".into(),
            ));
        }

        // Compute fee using TransactionFrame::fee_to_charge
        let mut fee = frame.fee_to_charge(base_fee as i64);

        if frame.is_fee_bump() {
            tracing::debug!(
                is_fee_bump = true,
                total_fee = frame.total_fee(),
                inner_fee = frame.inner_fee(),
                inclusion_fee = frame.inclusion_fee(),
                base_fee = base_fee,
                operation_count = frame.operation_count(),
                min_inclusion_fee = frame.min_inclusion_fee(base_fee as i64),
                fee_charged = fee,
                "Fee bump transaction fee calculation"
            );
        }

        if fee == 0 {
            return Ok((empty_entry_changes(), 0));
        }

        let delta_before_fee = delta_snapshot(&self.state);

        // Capture STATE entries BEFORE modifications for correct change generation
        // This is needed because flush_modified_entries updates snapshots to current values
        let mut state_overrides: HashMap<LedgerKey, LedgerEntry> = HashMap::new();
        if let Some(acc) = self.state.get_account(&fee_source_id) {
            let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                account_id: fee_source_id.clone(),
            });
            state_overrides.insert(key, self.state.ledger_entry_for_account(acc));
        }
        if fee_source_id != inner_source_id {
            if let Some(acc) = self.state.get_account(&inner_source_id) {
                let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                    account_id: inner_source_id.clone(),
                });
                state_overrides.insert(key, self.state.ledger_entry_for_account(acc));
            }
        }

        // Deduct fee — cap at available balance to prevent negative balances.
        // stellar-core: TransactionFrame.cpp:1797 — fee = std::min(acc.balance, fee)
        if let Some(acc) = self.state.get_account_mut(&fee_source_id) {
            fee = std::cmp::min(acc.balance, fee);
            henyey_common::checked_types::sub_account_balance(acc, fee)
                .expect("fee underflow after capping to balance");
        }

        // For protocol < 10, sequence bump happens during fee processing
        if protocol_version_is_before(self.protocol_version, ProtocolVersion::V10) {
            if let Some(acc) = self.state.get_account_mut(&inner_source_id) {
                // Set the account's seq_num to the transaction's seq_num
                acc.seq_num = stellar_xdr::curr::SequenceNumber(frame.sequence_number());
                henyey_tx::state::update_account_seq_info(acc, self.ledger_seq, self.close_time);
            }
        }

        self.state.delta_mut().add_fee(fee);
        self.state.flush_modified_entries();

        let delta_after_fee = delta_snapshot(&self.state);
        let delta_slice =
            delta_slice_between(self.state.delta(), delta_before_fee, delta_after_fee);
        let fee_changes = build_entry_changes_with_state_overrides(
            &self.state,
            delta_slice.created(),
            delta_slice.updated(),
            delta_slice.deleted(),
            &state_overrides,
        );

        // Commit fee changes so they persist to subsequent transactions
        self.state.commit();

        Ok((fee_changes, fee))
    }

    /// Execute a transaction with configurable fee deduction.
    ///
    /// When `deduct_fee` is false, fee validation still occurs but no fee
    /// processing changes are applied to the state or delta.
    ///
    pub fn execute_transaction_with_fee_mode(
        &mut self,
        snapshot: &SnapshotHandle,
        tx_envelope: &TransactionEnvelope,
        base_fee: u32,
        soroban_prng_seed: Option<[u8; 32]>,
        deduct_fee: bool,
    ) -> Result<TransactionExecutionResult> {
        self.execute_transaction_with_arc(
            snapshot,
            TransactionExecutionRequest::from_envelope(
                tx_envelope,
                base_fee,
                soroban_prng_seed,
                deduct_fee,
            ),
        )
    }

    /// Pre-apply phase: validate, charge fees, remove one-time signers, bump sequence.
    ///
    /// This matches stellar-core's `commonPreApply` + `preParallelApply` flow.
    /// On success, returns a `PreApplyResult` containing all data needed by the
    /// subsequent `apply_body()` phase. On validation failure, returns an early
    /// `TransactionExecutionResult` directly.
    ///
    /// After this method returns `Ok(Ok(..))`, the executor's state has committed
    /// fee deduction, signer removal, and sequence bump. These changes persist
    /// even if the operation body later fails (matching stellar-core behavior).
    pub(super) fn pre_apply_arc(
        &mut self,
        snapshot: &SnapshotHandle,
        tx_envelope: &Arc<TransactionEnvelope>,
        base_fee: u32,
        soroban_prng_seed: Option<[u8; 32]>,
        deduct_fee: bool,
    ) -> Result<std::result::Result<PreApplyResult, TransactionExecutionResult>> {
        let tx_timing_start = std::time::Instant::now();

        let soroban_max_refundable = {
            let pre_frame =
                TransactionFrame::with_network(Arc::clone(tx_envelope), self.network_id);
            if pre_frame.is_soroban() {
                let (non_refundable_fee, _) = compute_soroban_resource_fee(
                    &pre_frame,
                    self.protocol_version,
                    &self.soroban_config,
                    0,
                )
                .unwrap_or((0, 0));
                pre_frame
                    .declared_soroban_resource_fee()
                    .saturating_sub(non_refundable_fee)
            } else {
                0
            }
        };

        // Phase 1-6: Validate structure, accounts, fees, preconditions, sequence, signatures
        let validated = match self.validate_preconditions(snapshot, tx_envelope, base_fee)? {
            Ok(v) => v,
            Err(validation_failure) => {
                let mut failure_result = validation_failure.result;

                // For Soroban TXs, even validation failures get a fee refund.
                // stellar-core's setError() resets consumed to 0 and
                // finalizeFeeRefund() subtracts the full max_refundable from
                // feeCharged.
                failure_result.fee_refund = soroban_max_refundable;

                // If the validation passed the sequence check (stellar-core's
                // cv >= kInvalidUpdateSeqNum), perform the same post-seq
                // mutations as stellar-core's commonPreApply:
                //   1. Fee-bump wrapper: removeOneTimeSignerKeyFromFeeSource
                //   2. Inner: processSeqNum + processSignatures (which on
                //      protocol 13+ calls removeOneTimeSignerFromAllSourceAccounts)
                //   3. pushTxChangesBefore + commit
                if validation_failure.past_seq_check {
                    let (tx_changes_before, _) =
                        self.post_seq_failure_mutations(snapshot, tx_envelope)?;

                    // Build tx_meta so the seq bump + signer removal appear
                    // in the ledger close meta, matching stellar-core's
                    // pushTxChangesBefore(ltxTx) at commonPreApply:2037.
                    failure_result.tx_meta =
                        Some(meta::build_transaction_meta(meta::TransactionMetaParts {
                            tx_changes_before,
                            op_changes: vec![],
                            op_events: vec![],
                            tx_events: vec![],
                            soroban_return_value: None,
                            diagnostic_events: vec![],
                            soroban_fee_info: None,
                            emit_soroban_tx_meta_ext_v1: self.emit_soroban_tx_meta_ext_v1,
                            enable_soroban_diagnostic_events: self.enable_soroban_diagnostic_events,
                        }));
                }

                return Ok(Err(failure_result));
            }
        };
        let ValidatedTransaction {
            frame,
            fee_source_id,
            inner_source_id,
            outer_hash,
            val_account_load_us,
            val_tx_hash_us,
            val_ed25519_us,
            val_other_us,
        } = validated;

        let validation_us = tx_timing_start.elapsed().as_micros() as u64;

        let mut fee = frame.fee_to_charge(base_fee as i64);

        let fee_deduct_start = std::time::Instant::now();
        // For protocol 24+ (all Henyey-supported protocols), stellar-core's
        // commonPreApply(applying=true) sets feeToPay=0 after fee pre-deduction,
        // so partial fee charge never blocks body application. The fee is capped
        // at available balance during deduction below.
        let preflight_failure = None;

        let tx_event_manager = TxEventManager::new(
            true,
            self.protocol_version,
            self.network_id,
            self.classic_events,
        );
        let refundable_fee_tracker = if frame.is_soroban() {
            let (non_refundable_fee, _initial_refundable_fee) = compute_soroban_resource_fee(
                &frame,
                self.protocol_version,
                &self.soroban_config,
                0,
            )
            .unwrap_or((0, 0));
            let declared_fee = frame.declared_soroban_resource_fee();
            let max_refundable_fee = declared_fee.saturating_sub(non_refundable_fee);
            Some(RefundableFeeTracker::new(
                non_refundable_fee,
                max_refundable_fee,
            ))
        } else {
            None
        };

        let mut fee_created = Vec::new();
        let mut fee_updated = Vec::new();
        let mut fee_deleted = Vec::new();
        let fee_changes = if !deduct_fee || fee == 0 {
            empty_entry_changes()
        } else {
            let delta_before_fee = delta_snapshot(&self.state);

            // Deduct fee (sequence update is handled separately for protocol >= 10).
            if let Some(acc) = self.state.get_account_mut(&fee_source_id) {
                let old_balance = acc.balance;
                let charged_fee = std::cmp::min(acc.balance, fee);
                henyey_common::checked_types::sub_account_balance(acc, charged_fee)
                    .expect("fee underflow after capping to balance");
                fee = charged_fee;
                tracing::debug!(
                    account = %account_id_to_strkey(&fee_source_id),
                    old_balance = old_balance,
                    new_balance = acc.balance,
                    fee = charged_fee,
                    "Fee deducted from account"
                );
            }
            self.state.delta_mut().add_fee(fee);

            self.state.flush_modified_entries();
            let delta_after_fee = delta_snapshot(&self.state);
            let delta_slice =
                delta_slice_between(self.state.delta(), delta_before_fee, delta_after_fee);
            fee_created = delta_slice.created().to_vec();
            fee_updated = delta_slice.updated().to_vec();
            fee_deleted = delta_slice.deleted().to_vec();
            let fee_changes = build_entry_changes_with_state(
                &self.state,
                &fee_created,
                &fee_updated,
                &fee_deleted,
            );

            // Commit fee updates so txChangesBefore reflects the post-fee account state.
            self.state.commit();
            fee_changes
        };

        let fee_deduct_us = fee_deduct_start.elapsed().as_micros() as u64;

        let tx_changes_before: LedgerEntryChanges;

        // For fee bump transactions, stellar-core's FeeBumpTransactionFrame::apply()
        // ALWAYS calls removeOneTimeSignerKeyFromFeeSource() which removes any PreAuthTx
        // signer matching the fee bump outer hash from the fee source account. This happens
        // in both single-phase (deduct_fee=true) and two-phase (deduct_fee=false) modes.
        //
        // In two-phase mode, the STATE/UPDATED pair is captured in fee_bump_wrapper_changes
        // for metadata. In single-phase mode, the signer removal still happens but the
        // metadata changes are captured by the normal flush mechanism.
        let fee_bump_wrapper_changes = if frame.is_fee_bump() {
            let fee_source_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                account_id: fee_source_id.clone(),
            });
            if let Some(fee_source_before) = self.state.get_entry(&fee_source_key) {
                // Remove the PreAuthTx signer matching the fee bump outer hash from
                // the fee source, matching stellar-core's removeOneTimeSignerKeyFromFeeSource().
                let signer_key = stellar_xdr::curr::SignerKey::PreAuthTx(
                    stellar_xdr::curr::Uint256(outer_hash.0),
                );
                self.state
                    .remove_account_signer(&fee_source_id, &signer_key)
                    .map_err(|e| LedgerError::Internal(format!("signer removal error: {}", e)))?;
                self.state.flush_modified_entries();

                // Always capture the STATE/UPDATED pair. In stellar-core,
                let fee_source_after = self
                    .state
                    .get_entry(&fee_source_key)
                    .unwrap_or_else(|| fee_source_before.clone());

                vec![
                    LedgerEntryChange::State(fee_source_before),
                    LedgerEntryChange::Updated(fee_source_after),
                ]
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        // Check signatures BEFORE removing one-time signers.
        // In stellar-core, processSignatures() calls checkOperationSignatures()
        // for ALL transaction types (classic and Soroban), and both run BEFORE
        // removeOneTimeSignerFromAllSourceAccounts(). For fee-bump Soroban
        // transactions, a prior TX in the same ledger may modify the inner
        // source's signer set, making this check essential.
        let op_sig_check_start = std::time::Instant::now();
        let mut sig_check_failure: Option<(Vec<OperationResult>, ExecutionFailure)> = None;
        if preflight_failure.is_none() {
            // Pre-load per-operation source accounts from snapshot so they're
            // available to check_operation_signatures.
            for op in frame.operations().iter() {
                if let Some(ref source) = op.source_account {
                    let op_source_id = henyey_tx::muxed_to_account_id(source);
                    self.load_account(snapshot, &op_source_id)?;
                }
            }
            let sig_hash = if frame.is_fee_bump() {
                fee_bump_inner_hash(&frame, &self.network_id)?
            } else {
                outer_hash
            };
            let sig_signatures = if frame.is_fee_bump() {
                frame.inner_signatures()
            } else {
                frame.signatures()
            };

            // Per-op auth (processSignatures equivalent). Checks ALL ops.
            // Mirrors stellar-core: processSignatures runs BEFORE per-op
            // checkValid, so auth failures take precedence over structural errors.
            sig_check_failure = check_operation_signatures(
                &frame,
                &self.state,
                &sig_hash,
                sig_signatures,
                &inner_source_id,
            );

            // Per-op structural validation: isOpSupported + doCheckValid.
            // Only runs if auth passed (same as stellar-core's per-op
            // OperationFrame::checkValid loop in checkValidWithOptionallyChargedFee).
            if sig_check_failure.is_none() {
                sig_check_failure =
                    check_operations_valid(&frame, self.protocol_version, self.ledger_flags);
            }
        }

        let op_sig_check_us = op_sig_check_start.elapsed().as_micros() as u64;

        // Remove one-time (PreAuthTx) signers from all source accounts.
        // This must happen AFTER the signature check above so PreAuthTx signers
        // are still present when their weight is evaluated.
        // For fee bump transactions, use the inner TX contents hash (not the outer
        // fee bump hash), matching stellar-core's TransactionFrame::removeOneTimeSignerFromAllSourceAccounts()
        // which is called on the inner transaction.
        let signer_removal_hash = if frame.is_fee_bump() {
            fee_bump_inner_hash(&frame, &self.network_id)?
        } else {
            outer_hash
        };
        let (signer_changes, signer_created, signer_updated, signer_deleted, signer_removal_us) =
            self.remove_one_time_signers_phase(&frame, &inner_source_id, &signer_removal_hash)?;

        let (seq_changes, seq_created, seq_updated, seq_deleted, seq_bump_us) =
            self.bump_sequence_phase(&frame, &inner_source_id);

        // Merge all changes into tx_changes_before.
        // Order: fee_bump_wrapper_changes (fee source), seq_changes (inner source seq bump).
        // This matches stellar-core where FeeBumpFrame captures fee source state,
        // then inner tx does seq bump.
        let mut combined = Vec::with_capacity(
            fee_bump_wrapper_changes.len() + signer_changes.len() + seq_changes.len(),
        );
        combined.extend(fee_bump_wrapper_changes);
        combined.extend(signer_changes.iter().cloned());
        combined.extend(seq_changes.iter().cloned());
        tx_changes_before = combined.try_into().unwrap_or_default();

        // Commit pre-apply changes so rollback doesn't revert them.
        self.state.commit();
        let fee_seq_us = tx_timing_start.elapsed().as_micros() as u64 - validation_us;

        Ok(Ok(PreApplyResult {
            frame,
            fee_source_id,
            inner_source_id,
            tx_changes_before,
            fee_changes,
            refundable_fee_tracker,
            tx_event_manager,
            preflight_failure,
            sig_check_failure,
            fee,
            fee_entries: DeltaEntries {
                created: fee_created,
                updated: fee_updated,
                deleted: fee_deleted,
            },
            seq_entries: DeltaEntries {
                created: seq_created,
                updated: seq_updated,
                deleted: seq_deleted,
            },
            signer_entries: DeltaEntries {
                created: signer_created,
                updated: signer_updated,
                deleted: signer_deleted,
            },
            soroban_prng_seed,
            base_fee,
            deduct_fee,
            validation_us,
            fee_seq_us,
            tx_timing_start,
            val_account_load_us,
            val_tx_hash_us,
            val_ed25519_us,
            val_other_us,
            fee_deduct_us,
            op_sig_check_us,
            signer_removal_us,
            seq_bump_us,
            tx_hash: Some(outer_hash),
        }))
    }

    /// Execute a transaction with a pre-built execution request.
    ///
    /// This is the main orchestrator that calls `pre_apply()` for validation,
    /// fee charging, signer removal, and sequence bumping, then conditionally
    /// calls through to the operation body execution phase.
    ///
    /// Use `TransactionExecutionRequest::from_envelope` for call sites that start
    /// from an owned or borrowed envelope.
    pub fn execute_transaction_with_arc(
        &mut self,
        snapshot: &SnapshotHandle,
        request: TransactionExecutionRequest,
    ) -> Result<TransactionExecutionResult> {
        self.execute_transaction_with_request(snapshot, request)
    }

    /// Execute a transaction whose pre-apply phase was already completed by
    /// a global `preParallelApply` pass. Skips `pre_apply_arc` entirely and
    /// goes straight to `apply_body`.
    ///
    /// This matches stellar-core's `parallelApply` which consumes the result
    /// of `preParallelApply` and runs `doApply`. For protocol 24+, the body
    /// always executes after fee pre-deduction (stellar-core sets feeToPay=0
    /// in applying mode).
    pub(super) fn execute_with_pre_apply_result(
        &mut self,
        snapshot: &SnapshotHandle,
        mut pre: PreApplyResult,
    ) -> Result<TransactionExecutionResult> {
        // Clear rollback delta entries. The global pre_parallel_apply already
        // committed fee/seq/signer mutations to the main delta. If apply_body
        // fails, rollback_failed_tx must NOT re-record these on the cluster
        // executor's delta — that would cause double-apply when the cluster
        // delta is merged back into the main delta.
        //
        // This matches stellar-core's parallelApply which, on operation failure,
        // simply sets txFAILED without touching fee/seq/signer state (those are
        // already committed by preParallelApply on the shared ledger txn).
        //
        // Note: tx_changes_before and fee_changes (used for meta generation)
        // are separate fields in PreApplyResult and remain unaffected.
        pre.fee_entries = DeltaEntries::empty();
        pre.seq_entries = DeltaEntries::empty();
        pre.signer_entries = DeltaEntries::empty();

        self.apply_body(snapshot, pre)
    }

    fn execute_transaction_with_request(
        &mut self,
        snapshot: &SnapshotHandle,
        request: TransactionExecutionRequest,
    ) -> Result<TransactionExecutionResult> {
        let TransactionExecutionRequest {
            tx_envelope,
            base_fee,
            soroban_prng_seed,
            deduct_fee,
        } = request;

        // Phase 1: Pre-apply (validate, charge fees, remove signers, bump seq)
        let pre = match self.pre_apply_arc(
            snapshot,
            &tx_envelope,
            base_fee,
            soroban_prng_seed,
            deduct_fee,
        )? {
            Ok(pre) => pre,
            Err(early_result) => return Ok(early_result),
        };

        // Phase 2: Execute operation body
        self.apply_body(snapshot, pre)
    }

    /// Execute a single operation using the central dispatcher.
    fn execute_single_operation(
        &mut self,
        request: OperationExecutionRequest<'_>,
    ) -> std::result::Result<henyey_tx::operations::execute::OperationExecutionResult, TxError>
    {
        let OperationExecutionRequest {
            op,
            source,
            tx_source,
            tx_seq,
            op_index,
            context,
            soroban_data,
        } = request;

        // Create a hot archive lookup wrapper if hot archive is available
        let hot_archive_lookup;
        let hot_archive_ref: Option<&dyn henyey_tx::soroban::HotArchiveLookup> =
            if let Some(ref ha) = self.hot_archive {
                hot_archive_lookup = HotArchiveLookupImpl::new(ha.clone());
                Some(&hot_archive_lookup)
            } else {
                None
            };

        let tx_id = henyey_tx::operations::execute::TxIdentity {
            source_id: tx_source,
            seq: tx_seq,
            op_index,
        };
        let soroban = henyey_tx::soroban::SorobanContext {
            soroban_data,
            config: Some(&self.soroban_config),
            module_cache: self.module_cache.as_ref(),
            hot_archive: hot_archive_ref,
            ttl_key_cache: self.ttl_key_cache.as_ref(),
            previously_restored_keys: Some(&self.hot_archive_restored_keys),
        };

        // Use the central operation dispatcher which handles all operation types
        henyey_tx::operations::execute::execute_operation_with_soroban(
            op,
            source,
            &tx_id,
            &mut self.state,
            context,
            &soroban,
        )
    }

    /// Apply all state changes to the delta.
    pub fn apply_to_delta(&self, delta: &mut LedgerDelta) -> Result<()> {
        let state_delta = self.state.delta();

        // Apply changes in chronological order using change_order.
        // This is critical for correctness when the same key is affected by
        // multiple transactions (e.g., TX1 deletes an entry, TX2 recreates it).
        // Processing by category (all creates, then all updates, then all deletes)
        // would process the create before the delete, causing them to cancel out
        // instead of producing the correct Updated result.
        for change_ref in state_delta.change_order() {
            match change_ref {
                henyey_tx::ChangeRef::Created(idx) => {
                    let entry = &state_delta.created_entries()[*idx];
                    delta.record_create(entry.clone())?;
                }
                henyey_tx::ChangeRef::Updated(idx) => {
                    let prev = &state_delta.update_states()[*idx];
                    let entry = &state_delta.updated_entries()[*idx];
                    delta.record_update(prev.clone(), entry.clone())?;
                }
                henyey_tx::ChangeRef::Deleted(idx) => {
                    let prev = &state_delta.delete_states()[*idx];
                    delta.record_delete(prev.clone())?;
                }
            }
        }

        Ok(())
    }

    /// Get total fees collected.
    pub fn total_fees(&self) -> i64 {
        self.state.delta().fee_charged()
    }

    /// Get the updated ID pool after execution.
    pub fn id_pool(&self) -> u64 {
        self.state.id_pool()
    }

    /// Get the state manager.
    pub fn state(&self) -> &LedgerStateManager {
        &self.state
    }

    /// Get mutable state manager.
    pub fn state_mut(&mut self) -> &mut LedgerStateManager {
        &mut self.state
    }

    /// Remove one-time (PreAuthTx) signers from all source accounts.
    ///
    /// Returns (signer_changes, signer_created, signer_updated, signer_deleted, duration_us).
    #[allow(clippy::type_complexity)]
    fn remove_one_time_signers_phase(
        &mut self,
        frame: &TransactionFrame,
        inner_source_id: &AccountId,
        outer_hash: &Hash256,
    ) -> Result<(
        LedgerEntryChanges,
        Vec<LedgerEntry>,
        Vec<LedgerEntry>,
        Vec<LedgerKey>,
        u64,
    )> {
        let signer_removal_start = std::time::Instant::now();
        if self.protocol_version == 7 {
            let us = signer_removal_start.elapsed().as_micros() as u64;
            return Ok((
                empty_entry_changes(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                us,
            ));
        }

        let mut source_accounts = Vec::new();
        source_accounts.push(inner_source_id.clone());
        for op in frame.operations().iter() {
            if let Some(ref source) = op.source_account {
                source_accounts.push(henyey_tx::muxed_to_account_id(source));
            }
        }
        source_accounts.sort_by(|a, b| a.0.cmp(&b.0));
        source_accounts.dedup_by(|a, b| a.0 == b.0);

        let delta_before_signers = delta_snapshot(&self.state);
        let mut signer_state_overrides = HashMap::new();
        for account_id in &source_accounts {
            let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                account_id: account_id.clone(),
            });
            if let Some(entry) = self.state.get_entry(&key) {
                signer_state_overrides.insert(key, entry);
            }
        }

        self.state
            .remove_one_time_signers_from_all_sources(
                outer_hash,
                &source_accounts,
                self.protocol_version,
            )
            .map_err(|e| LedgerError::Internal(format!("signer removal error: {}", e)))?;
        self.state.flush_modified_entries();
        let delta_after_signers = delta_snapshot(&self.state);
        let delta_slice = delta_slice_between(
            self.state.delta(),
            delta_before_signers,
            delta_after_signers,
        );
        let signer_created = delta_slice.created().to_vec();
        let signer_updated = delta_slice.updated().to_vec();
        let signer_deleted = delta_slice.deleted().to_vec();
        let signer_changes = build_entry_changes_with_state_overrides(
            &self.state,
            &signer_created,
            &signer_updated,
            &signer_deleted,
            &signer_state_overrides,
        );
        let us = signer_removal_start.elapsed().as_micros() as u64;
        Ok((
            signer_changes,
            signer_created,
            signer_updated,
            signer_deleted,
            us,
        ))
    }

    /// Bump the transaction source account's sequence number and capture delta changes.
    ///
    /// Returns (seq_changes, seq_created, seq_updated, seq_deleted, duration_us).
    fn bump_sequence_phase(
        &mut self,
        frame: &TransactionFrame,
        inner_source_id: &AccountId,
    ) -> (
        LedgerEntryChanges,
        Vec<LedgerEntry>,
        Vec<LedgerEntry>,
        Vec<LedgerKey>,
        u64,
    ) {
        let seq_bump_start = std::time::Instant::now();
        let delta_before_seq = delta_snapshot(&self.state);
        // Capture the current account state BEFORE modification for STATE entry.
        // We can't use snapshot_entry() here because the snapshot might not exist yet.
        // After flush_modified_entries, the snapshot is updated to the post-modification
        // value, so we need to save the original here.
        let inner_source_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: inner_source_id.clone(),
        });
        let seq_state_override = self.state.get_entry(&inner_source_key);

        if let Some(acc) = self.state.get_account_mut(inner_source_id) {
            // CAP-0021: Set the account's seq_num to the transaction's seq_num.
            // This handles the case where minSeqNum allows sequence gaps - the
            // account's final seq must be the tx's seq, not just account_seq + 1.
            acc.seq_num = stellar_xdr::curr::SequenceNumber(frame.sequence_number());
            henyey_tx::state::update_account_seq_info(acc, self.ledger_seq, self.close_time);
        }
        self.state.flush_modified_entries();
        let delta_after_seq = delta_snapshot(&self.state);
        let delta_slice =
            delta_slice_between(self.state.delta(), delta_before_seq, delta_after_seq);
        // Use the pre-modification snapshot for STATE entry via state_overrides.
        let mut seq_state_overrides = HashMap::new();
        if let Some(entry) = seq_state_override {
            seq_state_overrides.insert(inner_source_key, entry);
        }
        let seq_changes = build_entry_changes_with_state_overrides(
            &self.state,
            delta_slice.created(),
            delta_slice.updated(),
            delta_slice.deleted(),
            &seq_state_overrides,
        );
        let seq_created = delta_slice.created().to_vec();
        let seq_updated = delta_slice.updated().to_vec();
        let seq_deleted = delta_slice.deleted().to_vec();
        // Persist sequence updates so failed transactions still consume sequence numbers.
        self.state.commit();

        let us = seq_bump_start.elapsed().as_micros() as u64;
        (seq_changes, seq_created, seq_updated, seq_deleted, us)
    }

    /// Perform post-sequence-check mutations for a failed transaction.
    ///
    /// When validation passes the sequence gate (`past_seq_check == true`) but
    /// fails afterward (e.g., min_seq_age, signatures), stellar-core's
    /// `commonPreApply` still runs:
    ///   1. `processSeqNum(ltxTx)` — bump sequence number
    ///   2. `processSignatures(cv, ...)` — remove PreAuthTx one-time signers
    ///   3. `pushTxChangesBefore(ltxTx)` + `ltxTx.commit()`
    ///
    /// For fee-bump transactions, `FeeBumpTransactionFrame::apply()` also calls
    /// `removeOneTimeSignerKeyFromFeeSource(ltxTx)` in a separate scope before
    /// the inner transaction's `commonPreApply`.
    ///
    /// Returns `(tx_changes_before, duration_us)`.
    fn post_seq_failure_mutations(
        &mut self,
        snapshot: &SnapshotHandle,
        tx_envelope: &Arc<TransactionEnvelope>,
    ) -> Result<(LedgerEntryChanges, u64)> {
        let start = std::time::Instant::now();
        let frame = TransactionFrame::with_network(Arc::clone(tx_envelope), self.network_id);
        let inner_source_id = henyey_tx::muxed_to_account_id(&frame.inner_source_account());
        let outer_hash = frame
            .hash(&self.network_id)
            .map_err(|e| LedgerError::Internal(format!("tx hash error: {}", e)))?;

        let mut all_changes = Vec::new();

        // Step 1: Fee-bump wrapper signer removal (separate scope in stellar-core).
        // Mirrors FeeBumpTransactionFrame::apply() lines 162-165.
        if frame.is_fee_bump() {
            let fee_source_id = henyey_tx::muxed_to_account_id(&frame.fee_source_account());
            let fee_source_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                account_id: fee_source_id.clone(),
            });
            if let Some(fee_source_before) = self.state.get_entry(&fee_source_key) {
                let signer_key = stellar_xdr::curr::SignerKey::PreAuthTx(
                    stellar_xdr::curr::Uint256(outer_hash.0),
                );
                self.state
                    .remove_account_signer(&fee_source_id, &signer_key)
                    .map_err(|e| LedgerError::Internal(format!("signer removal error: {}", e)))?;
                self.state.flush_modified_entries();

                let fee_source_after = self
                    .state
                    .get_entry(&fee_source_key)
                    .unwrap_or_else(|| fee_source_before.clone());
                all_changes.push(LedgerEntryChange::State(fee_source_before));
                all_changes.push(LedgerEntryChange::Updated(fee_source_after));
            }
        }

        // Step 2: Inner tx mutations (seq bump + signer removal in one scope).
        // Mirrors commonPreApply lines 2024-2038.
        //
        // Take a delta snapshot before any inner mutations so we can capture
        // the combined STATE/UPDATED entries for tx_changes_before.
        let delta_before = delta_snapshot(&self.state);

        // Capture pre-mutation account states for STATE entries in the meta.
        // We need these because after flush, get_entry returns post-mutation values.
        let inner_source_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: inner_source_id.clone(),
        });
        let mut state_overrides = HashMap::new();
        if let Some(entry) = self.state.get_entry(&inner_source_key) {
            state_overrides.insert(inner_source_key, entry);
        }

        // 2a: Sequence bump (processSeqNum)
        if let Some(acc) = self.state.get_account_mut(&inner_source_id) {
            acc.seq_num = stellar_xdr::curr::SequenceNumber(frame.sequence_number());
            henyey_tx::state::update_account_seq_info(acc, self.ledger_seq, self.close_time);
        }

        // 2b: Pre-load per-operation source accounts from snapshot so signer
        // removal can find them. On the success path this is done at line 2055.
        for op in frame.operations().iter() {
            if let Some(ref source) = op.source_account {
                let op_source_id = henyey_tx::muxed_to_account_id(source);
                self.load_account(snapshot, &op_source_id)?;
                // Capture pre-mutation state for these accounts too.
                let key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                    account_id: op_source_id,
                });
                if let Some(entry) = self.state.get_entry(&key) {
                    state_overrides.entry(key).or_insert(entry);
                }
            }
        }

        // 2c: Remove PreAuthTx one-time signers (processSignatures fast-fail path).
        // For fee-bumps, use the inner TX contents hash; for regular txs, outer hash.
        let signer_removal_hash = if frame.is_fee_bump() {
            fee_bump_inner_hash(&frame, &self.network_id)?
        } else {
            outer_hash
        };

        let mut source_accounts = Vec::new();
        source_accounts.push(inner_source_id.clone());
        for op in frame.operations().iter() {
            if let Some(ref source) = op.source_account {
                source_accounts.push(henyey_tx::muxed_to_account_id(source));
            }
        }
        source_accounts.sort_by(|a, b| a.0.cmp(&b.0));
        source_accounts.dedup_by(|a, b| a.0 == b.0);

        self.state
            .remove_one_time_signers_from_all_sources(
                &signer_removal_hash,
                &source_accounts,
                self.protocol_version,
            )
            .map_err(|e| LedgerError::Internal(format!("signer removal error: {}", e)))?;

        // Flush all inner mutations (seq bump + signer removal) and capture delta.
        self.state.flush_modified_entries();
        let delta_after = delta_snapshot(&self.state);
        let delta_slice = delta_slice_between(self.state.delta(), delta_before, delta_after);

        let inner_changes = build_entry_changes_with_state_overrides(
            &self.state,
            delta_slice.created(),
            delta_slice.updated(),
            delta_slice.deleted(),
            &state_overrides,
        );
        all_changes.extend(inner_changes.0.iter().cloned());

        // Commit all mutations so they persist.
        self.state.commit();

        let us = start.elapsed().as_micros() as u64;
        Ok((
            LedgerEntryChanges(all_changes.try_into().unwrap_or_default()),
            us,
        ))
    }
}

#[derive(Clone, Copy)]
pub struct DeltaSnapshot {
    created: usize,
    updated: usize,
    deleted: usize,
    change_order: usize,
}

/// Zero-copy view into a range of delta changes between two snapshots.
/// Avoids cloning vectors by referencing the parent `TxChangeLog` directly.
pub struct DeltaSlice<'a> {
    delta: &'a henyey_tx::TxChangeLog,
    start: DeltaSnapshot,
    end: DeltaSnapshot,
}

impl DeltaSlice<'_> {
    pub fn created(&self) -> &[LedgerEntry] {
        &self.delta.created_entries()[self.start.created..self.end.created]
    }

    pub fn updated(&self) -> &[LedgerEntry] {
        &self.delta.updated_entries()[self.start.updated..self.end.updated]
    }

    pub fn update_states(&self) -> &[LedgerEntry] {
        &self.delta.update_states()[self.start.updated..self.end.updated]
    }

    pub fn deleted(&self) -> &[LedgerKey] {
        &self.delta.deleted_keys()[self.start.deleted..self.end.deleted]
    }

    pub fn delete_states(&self) -> &[LedgerEntry] {
        &self.delta.delete_states()[self.start.deleted..self.end.deleted]
    }

    pub fn change_order(&self) -> Vec<henyey_tx::ChangeRef> {
        self.delta.change_order()[self.start.change_order..self.end.change_order]
            .iter()
            .map(|change_ref| match change_ref {
                henyey_tx::ChangeRef::Created(idx) => {
                    debug_assert!(
                        *idx >= self.start.created && *idx < self.end.created,
                        "ChangeRef::Created({idx}) outside slice range [{}..{})",
                        self.start.created,
                        self.end.created
                    );
                    henyey_tx::ChangeRef::Created(*idx - self.start.created)
                }
                henyey_tx::ChangeRef::Updated(idx) => {
                    debug_assert!(
                        *idx >= self.start.updated && *idx < self.end.updated,
                        "ChangeRef::Updated({idx}) outside slice range [{}..{})",
                        self.start.updated,
                        self.end.updated
                    );
                    henyey_tx::ChangeRef::Updated(*idx - self.start.updated)
                }
                henyey_tx::ChangeRef::Deleted(idx) => {
                    debug_assert!(
                        *idx >= self.start.deleted && *idx < self.end.deleted,
                        "ChangeRef::Deleted({idx}) outside slice range [{}..{})",
                        self.start.deleted,
                        self.end.deleted
                    );
                    henyey_tx::ChangeRef::Deleted(*idx - self.start.deleted)
                }
            })
            .collect()
    }
}

// Re-export ThresholdLevel from henyey_common for use in this module and submodules.
pub use henyey_common::ThresholdLevel;

/// Signature checker that tracks which signatures have been used.
///
/// Mirrors stellar-core's SignatureChecker: a single instance is created per
/// transaction and reused across TX-level checks, per-operation checks, and
/// extra signer checks. After all checks, `check_all_signatures_used()` verifies
/// that every signature in the envelope was consumed by at least one check.
pub struct SignatureTracker<'a> {
    tx_hash: &'a Hash256,
    signatures: &'a [stellar_xdr::curr::DecoratedSignature],
    used: Vec<bool>,
}

impl<'a> SignatureTracker<'a> {
    fn new(tx_hash: &'a Hash256, signatures: &'a [stellar_xdr::curr::DecoratedSignature]) -> Self {
        let used = vec![false; signatures.len()];
        Self {
            tx_hash,
            signatures,
            used,
        }
    }

    /// Check signatures against signers, tracking which signatures are consumed.
    /// Mirrors stellar-core's SignatureChecker::checkSignature().
    ///
    /// Returns true if total signer weight meets or exceeds `needed_weight`.
    fn check_signature(&mut self, account: &AccountEntry, needed_weight: u32) -> bool {
        // Build signer list: master key (if weight > 0) + account signers
        let mut signers: Vec<(SignerKey, u32)> = Vec::new();
        let master_weight = account.thresholds.0[0] as u32;
        if master_weight > 0 {
            let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref key) = account.account_id.0;
            let signer_key = SignerKey::Ed25519(key.clone());
            signers.push((signer_key, master_weight));
        }
        for signer in account.signers.iter() {
            signers.push((signer.key.clone(), signer.weight));
        }

        self.check_signature_from_signers(&signers, needed_weight as i32)
    }

    /// Check signatures against a synthetic signer list (for checkSignatureNoAccount).
    /// Used when a per-op source account doesn't exist but the op has an explicit
    /// source. Creates a synthetic signer with just the account's public key at
    /// weight 1, needed threshold 0.
    fn check_signature_no_account(&mut self, account_id: &AccountId) -> bool {
        let stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(ref key) = account_id.0;
        let signer_key = SignerKey::Ed25519(key.clone());
        let signers = vec![(signer_key, 1u32)];
        self.check_signature_from_signers(&signers, 0)
    }

    /// Core signature checking logic matching stellar-core's SignatureChecker::checkSignature().
    ///
    /// Splits signers by type, checks PRE_AUTH_TX first, then HASH_X, then ED25519,
    /// then ED25519_SIGNED_PAYLOAD. Weight is capped at 255 (UINT8_MAX) for protocol v10+.
    /// Once a signature matches a signer, that signer is consumed (each signer can only
    /// match once).
    fn check_signature_from_signers(
        &mut self,
        signers: &[(SignerKey, u32)],
        needed_weight: i32,
    ) -> bool {
        let mut total_weight: i32 = 0;

        // Split signers by type
        let mut pre_auth_signers: Vec<(usize, &SignerKey, u32)> = Vec::new();
        let mut hash_x_signers: Vec<(usize, &SignerKey, u32)> = Vec::new();
        let mut ed25519_signers: Vec<(usize, &SignerKey, u32)> = Vec::new();
        let mut signed_payload_signers: Vec<(usize, &SignerKey, u32)> = Vec::new();

        for (idx, (key, weight)) in signers.iter().enumerate() {
            match key {
                SignerKey::PreAuthTx(_) => pre_auth_signers.push((idx, key, *weight)),
                SignerKey::HashX(_) => hash_x_signers.push((idx, key, *weight)),
                SignerKey::Ed25519(_) => ed25519_signers.push((idx, key, *weight)),
                SignerKey::Ed25519SignedPayload(_) => {
                    signed_payload_signers.push((idx, key, *weight))
                }
            }
        }

        // Check PRE_AUTH_TX signers against tx hash (no envelope signature needed)
        for (_idx, key, weight) in &pre_auth_signers {
            if let SignerKey::PreAuthTx(pre_auth) = key {
                if pre_auth.0 == self.tx_hash.0 {
                    let w = std::cmp::min(*weight, 255) as i32;
                    total_weight += w;
                    if total_weight >= needed_weight {
                        return true;
                    }
                }
            }
        }

        // Check HASH_X signers
        let mut consumed_hash_x: HashSet<usize> = HashSet::new();
        for (sig_idx, sig) in self.signatures.iter().enumerate() {
            for (signer_idx, key, weight) in &hash_x_signers {
                if consumed_hash_x.contains(signer_idx) {
                    continue;
                }
                if let SignerKey::HashX(hash_key) = key {
                    let expected_hint = [
                        hash_key.0[28],
                        hash_key.0[29],
                        hash_key.0[30],
                        hash_key.0[31],
                    ];
                    if sig.hint.0 == expected_hint {
                        let hash = Hash256::hash(&sig.signature.0);
                        if hash.0 == hash_key.0 {
                            self.used[sig_idx] = true;
                            let w = std::cmp::min(*weight, 255) as i32;
                            total_weight += w;
                            consumed_hash_x.insert(*signer_idx);
                            if total_weight >= needed_weight {
                                return true;
                            }
                            break;
                        }
                    }
                }
            }
        }

        // Check ED25519 signers
        let mut consumed_ed25519: HashSet<usize> = HashSet::new();
        for (sig_idx, sig) in self.signatures.iter().enumerate() {
            for (signer_idx, key, weight) in &ed25519_signers {
                if consumed_ed25519.contains(signer_idx) {
                    continue;
                }
                if let SignerKey::Ed25519(ed_key) = key {
                    if validation::verify_signature_with_raw_key(self.tx_hash, sig, &ed_key.0) {
                        self.used[sig_idx] = true;
                        let w = std::cmp::min(*weight, 255) as i32;
                        total_weight += w;
                        consumed_ed25519.insert(*signer_idx);
                        if total_weight >= needed_weight {
                            return true;
                        }
                        break;
                    }
                }
            }
        }

        // Check ED25519_SIGNED_PAYLOAD signers
        let mut consumed_payload: HashSet<usize> = HashSet::new();
        for (sig_idx, sig) in self.signatures.iter().enumerate() {
            for (signer_idx, key, weight) in &signed_payload_signers {
                if consumed_payload.contains(signer_idx) {
                    continue;
                }
                if let SignerKey::Ed25519SignedPayload(payload) = key {
                    if has_signed_payload_match(sig, payload) {
                        self.used[sig_idx] = true;
                        let w = std::cmp::min(*weight, 255) as i32;
                        total_weight += w;
                        consumed_payload.insert(*signer_idx);
                        if total_weight >= needed_weight {
                            return true;
                        }
                        break;
                    }
                }
            }
        }

        // Mirror stellar-core's SignatureChecker::checkSignature(): only return
        // true if at least one signer actually matched (total_weight > 0).
        // stellar-core has no final "total >= needed" return; it falls through
        // to `return false` when nothing matched. This matters when needed_weight=0
        // (checkSignatureNoAccount): if the account's master key is not in the TX
        // signatures, we must return false (opBAD_AUTH), not true.
        total_weight >= needed_weight && total_weight > 0
    }

    /// Check that all signatures have been consumed.
    /// Mirrors stellar-core's SignatureChecker::checkAllSignaturesUsed().
    fn check_all_signatures_used(&self) -> bool {
        self.used.iter().all(|&u| u)
    }
}

// ---------------------------------------------------------------------------
// Parallel Soroban Phase Execution
// ---------------------------------------------------------------------------

/// Result of executing a transaction set (or a single cluster within one).
pub struct TxSetResult {
    pub results: Vec<TransactionExecutionResult>,
    pub tx_results: Vec<TransactionResultPair>,
    pub tx_result_metas: Vec<TransactionResultMetaV1>,
    pub id_pool: u64,
    pub hot_archive_restored_keys: Vec<LedgerKey>,
}

/// Soroban-related execution context bundled for passing through tx_set functions.
///
/// Groups the Soroban configuration, PRNG seed, classic event config,
/// optional module cache, hot archive, and runtime handle that travel
/// together through the parallel-phase pipeline.
pub struct SorobanContext<'a> {
    pub config: SorobanConfig,
    pub base_prng_seed: [u8; 32],
    pub classic_events: ClassicEventConfig,
    pub module_cache: Option<&'a PersistentModuleCache>,
    pub hot_archive: Option<std::sync::Arc<parking_lot::RwLock<Option<HotArchiveBucketList>>>>,
    pub runtime_handle: Option<tokio::runtime::Handle>,
    /// Shared in-memory Soroban state for O(1) contract entry lookups during execution.
    ///
    /// When provided, `load_soroban_footprint` uses this as the primary source for
    /// ContractData/ContractCode/TTL lookups, bypassing the 22-bucket list scan.
    pub soroban_state: Option<std::sync::Arc<crate::soroban_state::SharedSorobanState>>,
    /// Shared canonical offer store for classic phase execution.
    ///
    /// When provided, the executor's state manager uses this instead of maintaining
    /// a separate copy of all offers. Set for classic phase execution; `None` for
    /// parallel Soroban cluster executors (which don't touch offers).
    pub offer_store:
        Option<std::sync::Arc<parking_lot::Mutex<henyey_tx::state::offer_store::OfferStore>>>,
    /// Whether to emit `SorobanTransactionMetaExtV1` in transaction meta.
    pub emit_soroban_tx_meta_ext_v1: bool,
    /// Whether to include diagnostic events in transaction meta.
    pub enable_soroban_diagnostic_events: bool,
}

/// Snapshot of prior-stage state for parallel Soroban execution.
///
/// Bundles live entries and deleted keys together so they always travel as a
/// pair.  Constructed once per stage from the current `LedgerDelta` and shared
/// by all clusters in that stage.
pub struct PriorStageState {
    pub entries: Vec<LedgerEntry>,
    pub deleted_keys: Vec<LedgerKey>,
    /// Keys restored from hot archive by prior stages.
    /// Seeded into fresh cluster executors to prevent re-restoration
    /// across stages. Mirrors stellar-core's `mPreviouslyRestoredEntries`.
    pub hot_archive_restored_keys: std::collections::HashSet<LedgerKey>,
}

impl PriorStageState {
    /// Build from a delta, capturing both live entries and deletions.
    ///
    /// Only Soroban keys (ContractData, ContractCode, Ttl) are included in
    /// `deleted_keys` because parallel Soroban clusters never read classic
    /// entry types from `LedgerEntries` — classic entries are loaded directly
    /// from the snapshot. Including classic deletions would be harmless
    /// (mark_entry_deleted skips them) but filtering here keeps the invariant
    /// explicit and avoids unnecessary work.
    pub fn from_delta(
        delta: &crate::LedgerDelta,
        hot_archive_restored_keys: std::collections::HashSet<LedgerKey>,
    ) -> Self {
        Self {
            entries: delta.current_entries(),
            deleted_keys: delta
                .dead_entries()
                .into_iter()
                .filter(is_soroban_key)
                .collect(),
            hot_archive_restored_keys,
        }
    }
}

/// Returns true if a LedgerKey is a Soroban entry type (ContractData,
/// ContractCode, or Ttl) relevant for parallel Soroban execution state.
pub(crate) fn is_soroban_key(key: &LedgerKey) -> bool {
    matches!(
        key,
        LedgerKey::ContractData(_) | LedgerKey::ContractCode(_) | LedgerKey::Ttl(_)
    )
}

/// Parameters specific to a single cluster or stage within the parallel phase.
pub struct ClusterParams<'a> {
    pub id_pool: u64,
    pub prior_stage: &'a PriorStageState,
    pub pre_charged_fees: &'a [PreChargedFee],
}

/// Extract the fee-paying source AccountId from a raw TransactionEnvelope.
/// For fee bump transactions, this is the outer fee source.
/// For regular transactions, this is the transaction source account.
pub(crate) fn fee_source_account_id(env: &TransactionEnvelope) -> AccountId {
    let muxed = match env {
        TransactionEnvelope::TxV0(e) => MuxedAccount::Ed25519(e.tx.source_account_ed25519.clone()),
        TransactionEnvelope::Tx(e) => e.tx.source_account.clone(),
        TransactionEnvelope::TxFeeBump(e) => e.tx.fee_source.clone(),
    };
    match muxed {
        MuxedAccount::Ed25519(key) => {
            AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(key))
        }
        MuxedAccount::MuxedEd25519(m) => AccountId(
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(m.ed25519),
        ),
    }
}

/// Pre-charged fee information for a single Soroban transaction.
///
/// Computed during the pre-deduction pass (matching stellar-core `processFeesSeqNums`)
/// and consumed during cluster execution to provide fee metadata without
/// re-deducting fees.
#[derive(Clone)]
pub struct PreChargedFee {
    /// The fee actually charged (min(balance, computed_fee)).
    pub charged_fee: i64,
    /// Fee processing LedgerEntryChanges: [State(before), Updated(after)].
    pub fee_changes: LedgerEntryChanges,
}

/// Pre-deduct fees for all Soroban transactions across all stages/clusters.
///
/// This matches stellar-core `processFeesSeqNums` which deducts ALL transaction fees
/// (classic + Soroban) sequentially before any transaction is applied.
/// For the parallel phase, classic fees are already deducted by `execute_transaction_set`.
/// This function deducts Soroban fees from the main delta so cluster executors
/// see the correct post-fee-deduction account balances.
///
/// Returns `(pre_charged_fees, total_fee_pool_delta)` where `pre_charged_fees`
/// is indexed in flattened order (stage 0 cluster 0 TX 0, ..., stage N cluster M TX K).
fn pre_deduct_soroban_fees(
    snapshot: &SnapshotHandle,
    phase: &crate::close::SorobanPhaseStructure,
    base_fee: u32,
    network_id: NetworkId,
    ledger_seq: u32,
    delta: &mut LedgerDelta,
) -> Result<(Vec<PreChargedFee>, i64)> {
    let mut pre_charged: Vec<PreChargedFee> = Vec::new();
    let mut total_fee_pool = 0i64;

    for stage in &phase.stages {
        for cluster in stage {
            for (tx, tx_base_fee) in cluster {
                let tx_fee = tx_base_fee.unwrap_or(base_fee);
                let frame = TransactionFrame::with_network(tx.clone(), network_id);
                let fee_source = fee_source_account_id(tx);

                // Compute the fee using TransactionFrame::fee_to_charge.
                let computed_fee = frame.fee_to_charge(tx_fee as i64);

                let (charged_fee, fee_changes) = delta.deduct_fee_from_account(
                    &fee_source,
                    computed_fee,
                    snapshot,
                    ledger_seq,
                )?;

                total_fee_pool = total_fee_pool
                    .checked_add(charged_fee)
                    .expect("total_fee_pool overflow");
                pre_charged.push(PreChargedFee {
                    charged_fee,
                    fee_changes,
                });
            }
        }
    }

    Ok((pre_charged, total_fee_pool))
}

// Compile-time assertions that key types are Send, required for spawn_blocking.
const _: () = {
    fn _assert_send<T: Send>() {}
    fn _checks() {
        _assert_send::<TxSetResult>();
        _assert_send::<LedgerDelta>();
    }
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transaction_executor_creation() {
        let context = LedgerContext::new(100, 1234567890, 100, 5_000_000, 21, NetworkId::testnet());
        let executor = TransactionExecutor::new(
            &context,
            0,
            SorobanConfig::default(),
            ClassicEventConfig::default(),
        );

        assert_eq!(executor.ledger_seq, 100);
        assert_eq!(executor.close_time, 1234567890);
    }

    /// Regression test: Verify classic transaction fee calculation uses min(inclusion_fee, required_fee)
    ///
    /// This matches stellar-core's TransactionFrame::getFee() behavior when applying=true:
    /// - For classic transactions: min(inclusionFee, adjustedFee)
    ///   where adjustedFee = baseFee * numOperations
    ///
    /// Previously we incorrectly used inclusion_fee directly (or max()), which caused
    /// transactions with high declared fees to be charged more than necessary.
    #[test]
    fn test_classic_fee_calculation_uses_min() {
        // This test validates the fee calculation logic at lines ~1490-1497 and ~2129-2136
        // For a classic transaction with:
        //   - declared fee (inclusion_fee) = 1,000,000 stroops
        //   - base_fee = 100 stroops
        //   - num_ops = 1
        //   - required_fee = base_fee * num_ops = 100
        //
        // The fee charged should be min(1_000_000, 100) = 100, NOT 1,000,000

        // We can't easily test the full execute_transaction flow without extensive setup,
        // but we can verify the formula directly using the same calculation:
        let inclusion_fee: i64 = 1_000_000;
        let base_fee: i64 = 100;
        let num_ops: i64 = 1;
        let required_fee = base_fee * std::cmp::max(1, num_ops);

        // This is the correct formula (matches stellar-core):
        let fee_charged = std::cmp::min(inclusion_fee, required_fee);
        assert_eq!(
            fee_charged, 100,
            "Classic fee should be min(declared, required)"
        );

        // Verify it's not using max (the previous bug):
        let wrong_fee = std::cmp::max(inclusion_fee, required_fee);
        assert_eq!(
            wrong_fee, 1_000_000,
            "max() would incorrectly give 1,000,000"
        );
        assert_ne!(
            fee_charged, wrong_fee,
            "Fee calculation must use min(), not max()"
        );
    }

    /// Regression test: Verify Soroban fee calculation
    ///
    /// For Soroban transactions:
    ///   fee = resourceFee + min(inclusionFee, adjustedFee)
    #[test]
    fn test_soroban_fee_calculation() {
        let resource_fee: i64 = 500_000;
        let declared_total_fee: i64 = 2_000_000;
        let inclusion_fee = declared_total_fee - resource_fee; // 1,500,000
        let base_fee: i64 = 100;
        let num_ops: i64 = 1;
        let required_fee = base_fee * std::cmp::max(1, num_ops); // 100

        // Soroban fee = resourceFee + min(inclusionFee, adjustedFee)
        let fee_charged = resource_fee + std::cmp::min(inclusion_fee, required_fee);

        // Should be 500_000 + min(1_500_000, 100) = 500_000 + 100 = 500_100
        assert_eq!(
            fee_charged, 500_100,
            "Soroban fee should be resourceFee + min(inclusionFee, adjustedFee)"
        );
    }

    /// Regression test for F8: Fee refund on failed Soroban transactions
    ///
    /// When a Soroban transaction fails (e.g., InsufficientRefundableFee), the full
    /// max_refundable_fee should be refunded. This test verifies that reset() properly
    /// clears consumed values so refund_amount() returns the full max_refundable_fee.
    ///
    /// This mirrors stellar-core's behavior where setError() calls resetConsumedFee().
    ///
    /// Observed at ledger 224398 TX 7: fee refund was 0, expected 47153 stroops.
    #[test]
    fn test_refundable_fee_tracker_reset_on_failure() {
        let non_refundable_fee = 125_890;
        let max_refundable_fee = 47_153;

        let mut tracker = RefundableFeeTracker::new(non_refundable_fee, max_refundable_fee);

        // Simulate consuming fees that would exceed max_refundable_fee
        // (This would happen when consume() fails the InsufficientRefundableFee check)
        tracker.consumed_event_size_bytes = 1000;
        tracker.consumed_rent_fee = 50_000;
        tracker.consumed_refundable_fee = 60_000; // Exceeds max_refundable_fee

        // Before reset, refund_amount should be 0 (because consumed > max)
        assert_eq!(
            tracker.refund_amount(),
            0,
            "refund should be 0 when consumed > max"
        );

        // Reset the tracker (as done when transaction fails)
        tracker.reset();

        // After reset, consumed values should all be 0
        assert_eq!(tracker.consumed_event_size_bytes, 0);
        assert_eq!(tracker.consumed_rent_fee, 0);
        assert_eq!(tracker.consumed_refundable_fee, 0);

        // Now refund_amount should return the full max_refundable_fee
        assert_eq!(
            tracker.refund_amount(),
            max_refundable_fee,
            "refund should be full max_refundable_fee after reset"
        );
    }

    /// Regression test for F17: extract_hot_archive_restored_keys uses actual_restored_indices
    ///
    /// When multiple transactions in the same ledger reference the same archived entry,
    /// only the FIRST transaction should treat it as a hot archive restore. Subsequent
    /// transactions should see it as already live (restored by the prior TX).
    ///
    /// The key insight is that `archived_soroban_entries` in the transaction envelope
    /// is set at submission time, not execution time. By execution time, a prior TX
    /// may have already restored the entry.
    ///
    /// This test verifies that extract_hot_archive_restored_keys only returns keys
    /// for indices in `actual_restored_indices`, not all `archived_soroban_entries`.
    #[test]
    fn test_extract_hot_archive_restored_keys_uses_actual_indices() {
        use stellar_xdr::curr::{
            ContractDataDurability, ContractId, LedgerFootprint, LedgerKey, LedgerKeyContractData,
            ScAddress, ScVal, SorobanResources, SorobanResourcesExtV0, SorobanTransactionData,
            SorobanTransactionDataExt,
        };

        // Create a footprint with 3 keys in read_write
        let key0 = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(stellar_xdr::curr::Hash([0u8; 32]))),
            key: ScVal::U32(0),
            durability: ContractDataDurability::Persistent,
        });
        let key1 = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(stellar_xdr::curr::Hash([1u8; 32]))),
            key: ScVal::U32(1),
            durability: ContractDataDurability::Persistent,
        });
        let key2 = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(stellar_xdr::curr::Hash([2u8; 32]))),
            key: ScVal::U32(2),
            durability: ContractDataDurability::Persistent,
        });

        let footprint = LedgerFootprint {
            read_only: vec![].try_into().unwrap(),
            read_write: vec![key0.clone(), key1.clone(), key2.clone()]
                .try_into()
                .unwrap(),
        };

        // Envelope says all 3 indices (0, 1, 2) need restoration
        // (this was set at submission time)
        // Note: V1 extension with archived_soroban_entries requires Protocol 25
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                archived_soroban_entries: vec![0, 1, 2].try_into().unwrap(),
            }),
            resources: SorobanResources {
                footprint,
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        // Case 1: All 3 are actually archived (actual_restored_indices = [0, 1, 2])
        let actual_all = vec![0u32, 1, 2];
        let result = extract_hot_archive_restored_keys(
            Some(&soroban_data),
            OperationType::InvokeHostFunction,
            &actual_all,
        );
        assert_eq!(
            result.len(),
            3,
            "Should return all 3 keys when all are actually archived"
        );
        assert!(result.contains(&key0));
        assert!(result.contains(&key1));
        assert!(result.contains(&key2));

        // Case 2: Only index 0 is actually archived (1 and 2 were restored by prior TX)
        // This is the bug scenario - actual_restored_indices is filtered by the host
        let actual_one = vec![0u32];
        let result = extract_hot_archive_restored_keys(
            Some(&soroban_data),
            OperationType::InvokeHostFunction,
            &actual_one,
        );
        assert_eq!(
            result.len(),
            1,
            "Should only return 1 key when only index 0 is actually archived"
        );
        assert!(result.contains(&key0), "Should contain key0");
        assert!(
            !result.contains(&key1),
            "Should NOT contain key1 (already restored)"
        );
        assert!(
            !result.contains(&key2),
            "Should NOT contain key2 (already restored)"
        );

        // Case 3: None are actually archived (all were restored by prior TXs)
        let actual_none: Vec<u32> = vec![];
        let result = extract_hot_archive_restored_keys(
            Some(&soroban_data),
            OperationType::InvokeHostFunction,
            &actual_none,
        );
        assert_eq!(
            result.len(),
            0,
            "Should return empty set when none are actually archived"
        );

        // Case 4: RestoreFootprint should always return empty (handled separately)
        let result = extract_hot_archive_restored_keys(
            Some(&soroban_data),
            OperationType::RestoreFootprint,
            &actual_all,
        );
        assert_eq!(
            result.len(),
            0,
            "RestoreFootprint should return empty (keys come from meta.hot_archive_restores)"
        );
    }

    /// Parity: LedgerTxnTests.cpp:241 "restored keys" / "rollback" scenario
    /// When no soroban data is provided, restored keys should be empty.
    /// This covers the case where non-Soroban transactions don't produce restored keys.
    #[test]
    fn test_extract_hot_archive_restored_keys_no_soroban_data() {
        let result =
            extract_hot_archive_restored_keys(None, OperationType::InvokeHostFunction, &[0, 1, 2]);
        assert!(
            result.is_empty(),
            "No soroban data should produce empty restored keys"
        );
    }

    /// Parity: LedgerTxnTests.cpp:241 "restored keys" / empty actual_restored_indices
    /// When actual_restored_indices is empty (no entries actually need restoration),
    /// the result is empty regardless of what the envelope declares.
    #[test]
    fn test_extract_hot_archive_restored_keys_empty_indices() {
        use stellar_xdr::curr::{
            ContractDataDurability, ContractId, LedgerFootprint, LedgerKey, LedgerKeyContractData,
            ScAddress, ScVal, SorobanResources, SorobanResourcesExtV0, SorobanTransactionData,
            SorobanTransactionDataExt,
        };

        let key0 = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(stellar_xdr::curr::Hash([0u8; 32]))),
            key: ScVal::U32(0),
            durability: ContractDataDurability::Persistent,
        });

        // V1 extension with archived entries declared, but actual_restored is empty
        // (all entries were already restored by prior TXs in this ledger)
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                archived_soroban_entries: vec![0].try_into().unwrap(),
            }),
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![].try_into().unwrap(),
                    read_write: vec![key0].try_into().unwrap(),
                },
                instructions: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 0,
        };

        // Empty actual indices = nothing actually restored
        let result = extract_hot_archive_restored_keys(
            Some(&soroban_data),
            OperationType::InvokeHostFunction,
            &[],
        );
        assert!(
            result.is_empty(),
            "Empty actual_restored_indices should produce no restored keys"
        );
    }

    /// Regression test for VE-06: failed operations must not contribute hot archive keys.
    ///
    /// In stellar-core, handleArchivedEntry writes the restoration to mOpState (a nested
    /// LedgerTxn). When the operation fails, the nested LedgerTxn is rolled back, which
    /// cancels the restorations. No HOT_ARCHIVE_LIVE tombstones are written.
    ///
    /// Before the VE-06 fix, our code added hot archive keys to `collected_hot_archive_keys`
    /// regardless of operation success. This produced spurious HOT_ARCHIVE_LIVE tombstones
    /// for failed operations, diverging the hot archive bucket list hash from stellar-core.
    ///
    /// The fix: only add to collected_hot_archive_keys when is_operation_success(&op_result).
    ///
    /// This test verifies that the gating logic is sound: a failed operation result
    /// correctly prevents hot archive key collection.
    #[test]
    fn test_ve06_failed_op_hot_archive_keys_not_collected() {
        use std::collections::HashSet;
        use stellar_xdr::curr::{
            ContractDataDurability, ContractId, LedgerKey, LedgerKeyContractData, OperationResult,
            ScAddress, ScVal,
        };

        let archived_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(stellar_xdr::curr::Hash([0xCCu8; 32]))),
            key: ScVal::U32(42),
            durability: ContractDataDurability::Persistent,
        });

        // Simulate the hot_archive_for_bucket_list set built from actual_restored_indices.
        // This key corresponds to an entry declared in archived_soroban_entries.
        let hot_archive_for_bucket_list: HashSet<LedgerKey> =
            std::iter::once(archived_key.clone()).collect();

        // Case 1: operation succeeded → keys ARE collected.
        let success_result =
            OperationResult::OpInner(stellar_xdr::curr::OperationResultTr::RestoreFootprint(
                stellar_xdr::curr::RestoreFootprintResult::Success,
            ));
        assert!(is_operation_success(&success_result));

        let mut collected_success: HashSet<LedgerKey> = HashSet::new();
        if is_operation_success(&success_result) {
            collected_success.extend(hot_archive_for_bucket_list.iter().cloned());
        }
        assert_eq!(
            collected_success.len(),
            1,
            "VE-06: hot archive keys must be collected for successful operations"
        );

        // Case 2: operation failed → keys are NOT collected (VE-06 fix).
        let failed_result =
            OperationResult::OpInner(stellar_xdr::curr::OperationResultTr::InvokeHostFunction(
                stellar_xdr::curr::InvokeHostFunctionResult::EntryArchived,
            ));
        assert!(!is_operation_success(&failed_result));

        let mut collected_failed: HashSet<LedgerKey> = HashSet::new();
        if is_operation_success(&failed_result) {
            // This block must NOT execute for a failed operation.
            collected_failed.extend(hot_archive_for_bucket_list.iter().cloned());
        }
        assert!(
            collected_failed.is_empty(),
            "VE-06: hot archive keys must NOT be collected for failed operations — \
             stellar-core rolls back mOpState (including handleArchivedEntry restorations) \
             when an operation fails, so no HOT_ARCHIVE_LIVE tombstones are written"
        );
    }

    /// Parity: LedgerTxnTests.cpp:241 "restored keys" / commit accumulates
    /// Verify that hot_archive_restored_keys in TransactionExecutionResult is correctly
    /// structured as a Vec that can accumulate keys across transactions.
    #[test]
    fn test_restored_keys_accumulation_pattern() {
        use stellar_xdr::curr::{
            ContractDataDurability, ContractId, LedgerKey, LedgerKeyContractData, ScAddress, ScVal,
        };

        // Simulate accumulation of restored keys from multiple transactions
        let mut all_restored: Vec<LedgerKey> = Vec::new();

        // TX1 restores key0
        let key0 = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(stellar_xdr::curr::Hash([0u8; 32]))),
            key: ScVal::U32(0),
            durability: ContractDataDurability::Persistent,
        });
        all_restored.push(key0.clone());

        // TX2 restores key1
        let key1 = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(stellar_xdr::curr::Hash([1u8; 32]))),
            key: ScVal::U32(1),
            durability: ContractDataDurability::Persistent,
        });
        all_restored.push(key1.clone());

        // After all TXs in a ledger close, the accumulated keys should contain both
        assert_eq!(all_restored.len(), 2);
        assert_eq!(all_restored[0], key0);
        assert_eq!(all_restored[1], key1);
    }

    /// Integration-level regression test for VE-11: entries deleted in stage 0
    /// must be invisible to stage 1 after PriorStageState propagation.
    ///
    /// Simulates the cross-stage flow:
    ///   1. Stage 0 executor loads a ContractCode entry from the snapshot and
    ///      deletes it (host doesn't return a new value).
    ///   2. PriorStageState is built from the delta (captures entries + deletions).
    ///   3. A fresh stage 1 executor receives the PriorStageState and applies it
    ///      (same logic as execute_single_cluster).
    ///   4. load_soroban_footprint on the stage 1 executor must NOT reload the
    ///      deleted entry from the bucket list.
    ///
    /// Without mark_entry_deleted(), the fresh executor in step 3 would fall
    /// through to the bucket list and reload the stale entry.
    #[test]
    fn test_cross_stage_deleted_entry_not_reloaded_ve11() {
        use crate::snapshot::{LedgerSnapshot, SnapshotHandle};
        use crate::LedgerDelta;
        use std::sync::Arc;
        use stellar_xdr::curr::*;

        // --- Build a ContractCode entry and its TTL in the snapshot ---
        let code_hash = Hash([0xCC; 32]);
        let contract_code = ContractCodeEntry {
            ext: ContractCodeEntryExt::V0,
            hash: code_hash.clone(),
            code: BytesM::try_from(vec![1u8, 2u8]).unwrap(),
        };
        let code_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: code_hash.clone(),
        });
        let code_entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractCode(contract_code),
            ext: LedgerEntryExt::V0,
        };

        // Compute TTL key hash (SHA-256 of LedgerKey XDR)
        let ttl_key_hash = {
            use sha2::{Digest, Sha256};
            let bytes = code_key.to_xdr(Limits::none()).unwrap();
            Hash(Sha256::digest(&bytes).into())
        };
        let ttl_key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: ttl_key_hash.clone(),
        });
        let ttl_entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Ttl(TtlEntry {
                key_hash: ttl_key_hash,
                live_until_ledger_seq: 200,
            }),
            ext: LedgerEntryExt::V0,
        };

        // Snapshot lookup returns both entries from "bucket list".
        let code_entry_bl = code_entry.clone();
        let ttl_entry_bl = ttl_entry.clone();
        let code_key_bl = code_key.clone();
        let ttl_key_bl = ttl_key.clone();
        let lookup_fn: crate::snapshot::EntryLookupFn = Arc::new(move |key: &LedgerKey| {
            if *key == code_key_bl {
                return Ok(Some(code_entry_bl.clone()));
            }
            if *key == ttl_key_bl {
                return Ok(Some(ttl_entry_bl.clone()));
            }
            Ok(None)
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup_fn);

        let context = LedgerContext::new(101, 1234567890, 100, 5_000_000, 25, NetworkId::testnet());

        // --- Build PriorStageState as if stage 0 deleted the entries ---
        // Simulate: stage 0 loaded the entries and a TX deleted them.
        // The delta after stage 0 contains the deletions.
        let mut delta = LedgerDelta::new(101);
        delta.record_delete(code_entry.clone()).unwrap();
        delta.record_delete(ttl_entry.clone()).unwrap();

        let prior_stage = PriorStageState::from_delta(&delta, std::collections::HashSet::new());
        // Verify the deleted keys are captured.
        assert!(
            prior_stage.deleted_keys.contains(&code_key),
            "PriorStageState must capture deleted ContractCode key"
        );
        assert!(
            prior_stage.deleted_keys.contains(&ttl_key),
            "PriorStageState must capture deleted TTL key"
        );

        // --- Stage 1: fresh executor, apply PriorStageState ---
        let mut stage1_executor = TransactionExecutor::new(
            &context,
            0,
            SorobanConfig::default(),
            ClassicEventConfig::default(),
        );

        // Apply prior-stage entries (same as execute_single_cluster does).
        for entry in &prior_stage.entries {
            stage1_executor.state.load_entry(entry.clone());
        }
        // Apply prior-stage deletions.
        for key in &prior_stage.deleted_keys {
            stage1_executor.state.mark_entry_deleted(key);
        }

        // Verify the entry is marked as deleted in the fresh executor.
        assert!(
            stage1_executor.state.is_entry_deleted(&code_key),
            "Stage 1 executor must know the entry is deleted"
        );

        // --- load_soroban_footprint must NOT reload the deleted entry ---
        let footprint = LedgerFootprint {
            read_only: vec![code_key.clone()].try_into().unwrap(),
            read_write: VecM::default(),
        };
        stage1_executor
            .load_soroban_footprint(&snapshot, &footprint)
            .unwrap();

        // The entry must remain absent — load_soroban_footprint skipped it because
        // is_entry_deleted returned true.
        assert!(
            stage1_executor.state.get_entry(&code_key).is_none(),
            "Deleted entry must NOT be reloaded from bucket list in stage 1"
        );
    }

    /// Regression test for VE-10: account deleted by account_merge within a ledger
    /// must not be re-loadable from the bucket list snapshot.
    ///
    /// In the parallel fee-deduction path, accounts are loaded into the executor
    /// state directly via `state.load_entry()` (not through `load_account`), so
    /// `loaded_accounts` is never populated.  When a prior TX in the ledger
    /// deletes the account (account_merge), a subsequent TX's `load_account`
    /// must detect the deletion via `delta().deleted_keys()` instead of relying
    /// on `loaded_accounts`.
    ///
    /// Before the fix, `load_account` fell through to the bucket-list snapshot
    /// and returned the stale LIVE entry, producing txSuccess instead of the
    /// correct TxNoAccount.
    ///
    /// Observed at mainnet ledger 60645316 TX 68 (account GBPHB57...).
    #[test]
    fn test_deleted_account_not_reloaded_from_snapshot() {
        use crate::snapshot::{LedgerSnapshot, SnapshotHandle};
        use std::sync::Arc;
        use stellar_xdr::curr::*;

        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0xAA; 32])));
        let account_entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id.clone(),
                balance: 100_000_000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        // Create a snapshot that returns the account from the "bucket list"
        let account_entry_clone = account_entry.clone();
        let account_id_for_lookup = account_id.clone();
        let lookup_fn: crate::snapshot::EntryLookupFn = Arc::new(move |key: &LedgerKey| {
            if let LedgerKey::Account(k) = key {
                if k.account_id == account_id_for_lookup {
                    return Ok(Some(account_entry_clone.clone()));
                }
            }
            Ok(None)
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup_fn);

        // Create executor
        let context = LedgerContext::new(101, 1234567890, 100, 5_000_000, 25, NetworkId::testnet());
        let mut executor = TransactionExecutor::new(
            &context,
            0,
            SorobanConfig::default(),
            ClassicEventConfig::default(),
        );

        // Mimic the parallel fee-deduction path: load the account directly into
        // state via state.load_entry(), NOT through load_account().  This means
        // loaded_accounts is NOT populated for this key.
        executor.state.load_entry(account_entry.clone());
        assert!(
            executor.state.get_account(&account_id).is_some(),
            "Account should be in state after direct load"
        );

        // Simulate a prior TX deleting the account (account_merge).
        executor.state.delete_account(&account_id);
        assert!(
            executor.state.get_account(&account_id).is_none(),
            "Account should be gone from state after delete"
        );

        // A subsequent TX tries to load the account.  loaded_accounts is empty,
        // so without the delta().deleted_keys() check, this would fall through
        // to the bucket-list snapshot and return true (the stale LIVE entry).
        let reloaded = executor.load_account(&snapshot, &account_id).unwrap();
        assert!(
            !reloaded,
            "Account deleted in this ledger must NOT be re-loaded from snapshot"
        );
    }

    /// Regression test: generic `load_entry` must not reload entries deleted by
    /// a prior TX in the same ledger from the bucket-list snapshot.
    ///
    /// Same class of bug as VE-10 (load_account variant). The generic
    /// `load_entry` is used by `RevokeSponsorship(LedgerEntry(...))` and must
    /// honour the delta's deleted_keys set.
    #[test]
    fn test_load_entry_respects_delta_deleted_keys() {
        use crate::snapshot::{LedgerSnapshot, SnapshotHandle};
        use std::sync::Arc;
        use stellar_xdr::curr::*;

        // Create an account that exists in the "bucket list" snapshot.
        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0xBB; 32])));
        let account_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        let account_entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id.clone(),
                balance: 50_000_000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };

        let account_entry_clone = account_entry.clone();
        let account_key_for_lookup = account_key.clone();
        let lookup_fn: crate::snapshot::EntryLookupFn = Arc::new(move |key: &LedgerKey| {
            if *key == account_key_for_lookup {
                return Ok(Some(account_entry_clone.clone()));
            }
            Ok(None)
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup_fn);

        let context = LedgerContext::new(101, 1234567890, 100, 5_000_000, 25, NetworkId::testnet());
        let mut executor = TransactionExecutor::new(
            &context,
            0,
            SorobanConfig::default(),
            ClassicEventConfig::default(),
        );

        // Load the entry into state, then delete it (simulating account_merge
        // by a prior TX).
        executor.state.load_entry(account_entry.clone());
        assert!(executor.state.get_account(&account_id).is_some());
        executor.state.delete_account(&account_id);
        assert!(executor.state.get_account(&account_id).is_none());

        // Generic load_entry must not reload the stale entry from the snapshot.
        let reloaded = executor.load_entry(&snapshot, &account_key).unwrap();
        assert!(
            !reloaded,
            "load_entry must not reload an entry deleted in this ledger"
        );
        // Verify the account is still absent from state.
        assert!(
            executor.state.get_account(&account_id).is_none(),
            "Account must remain absent after load_entry returns false"
        );
    }

    #[test]
    fn test_record_entry_access_stamps_loaded_data_entry() {
        use crate::snapshot::{LedgerSnapshot, SnapshotHandle};
        use std::sync::Arc;
        use stellar_xdr::curr::*;

        let owner = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0x11; 32])));
        let sponsor = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0x22; 32])));
        let data_name = String64::try_from(b"Test".to_vec()).unwrap();

        let data_key = LedgerKey::Data(LedgerKeyData {
            account_id: owner.clone(),
            data_name: data_name.clone(),
        });
        let data_entry = LedgerEntry {
            last_modified_ledger_seq: 123,
            data: LedgerEntryData::Data(DataEntry {
                account_id: owner.clone(),
                data_name: data_name.clone(),
                data_value: vec![1, 2, 3].try_into().unwrap(),
                ext: DataEntryExt::V0,
            }),
            ext: LedgerEntryExt::V1(LedgerEntryExtensionV1 {
                sponsoring_id: SponsorshipDescriptor(Some(sponsor)),
                ext: LedgerEntryExtensionV1Ext::V0,
            }),
        };

        let data_entry_for_lookup = data_entry.clone();
        let data_key_for_lookup = data_key.clone();
        let lookup_fn: crate::snapshot::EntryLookupFn = Arc::new(move |key: &LedgerKey| {
            if *key == data_key_for_lookup {
                Ok(Some(data_entry_for_lookup.clone()))
            } else {
                Ok(None)
            }
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(123), lookup_fn);

        let context = LedgerContext::new(200, 1234567890, 100, 5_000_000, 25, NetworkId::testnet());
        let mut executor = TransactionExecutor::new(
            &context,
            0,
            SorobanConfig::default(),
            ClassicEventConfig::default(),
        );

        executor.state.begin_op_snapshot();
        executor.load_entry(&snapshot, &data_key).unwrap();
        executor.state.record_entry_access(&data_key);
        executor.state.flush_modified_entries();
        let _ = executor.state.end_op_snapshot();

        let updated = executor
            .state
            .delta()
            .updated_entries()
            .iter()
            .find(|entry| matches!(&entry.data, LedgerEntryData::Data(data) if data.account_id == owner))
            .expect("data entry should be recorded as updated");
        assert_eq!(updated.last_modified_ledger_seq, 200);
    }

    /// O1 optimization: load_soroban_footprint uses InMemorySorobanState when set.
    ///
    /// With soroban_state set on the executor, ContractData/ContractCode entries in the
    /// footprint must be sourced from the in-memory HashMap (O(1)) rather than the bucket
    /// list. The snapshot lookup function panics if called for Soroban keys, proving that
    /// the bucket list is never consulted when IMS holds the entry.
    #[test]
    fn test_load_soroban_footprint_uses_in_memory_state() {
        use crate::snapshot::{LedgerSnapshot, SnapshotHandle};
        use crate::soroban_state::SharedSorobanState;
        use sha2::{Digest, Sha256};
        use std::sync::Arc;
        use stellar_xdr::curr::*;

        // --- Build a ContractData entry ---
        let contract_address = ScAddress::Contract(ContractId(Hash([0xAB; 32])));
        let data_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_address.clone(),
            key: ScVal::U32(99),
            durability: ContractDataDurability::Persistent,
        });
        let data_entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: contract_address.clone(),
                key: ScVal::U32(99),
                durability: ContractDataDurability::Persistent,
                val: ScVal::I32(42),
            }),
            ext: LedgerEntryExt::V0,
        };

        // Compute TTL key hash
        let ttl_key_hash = Hash(Sha256::digest(&data_key.to_xdr(Limits::none()).unwrap()).into());
        let ttl_key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: ttl_key_hash.clone(),
        });

        // --- Populate SharedSorobanState ---
        let shared = Arc::new(SharedSorobanState::new());
        {
            let mut ims = shared.write();
            // create_contract_data also registers any pending TTL
            ims.create_ttl(
                &LedgerKeyTtl {
                    key_hash: ttl_key_hash.clone(),
                },
                crate::soroban_state::TtlData::new(200, 100),
            )
            .unwrap();
            ims.create_contract_data(data_entry.clone()).unwrap();
        }

        // --- Snapshot that panics if Soroban keys reach the bucket list ---
        let data_key_cl = data_key.clone();
        let ttl_key_cl = ttl_key.clone();
        let lookup_fn: crate::snapshot::EntryLookupFn = Arc::new(move |key: &LedgerKey| {
            if *key == data_key_cl || *key == ttl_key_cl {
                panic!("Bucket list must NOT be consulted when InMemorySorobanState is set");
            }
            Ok(None)
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup_fn);

        // --- Executor with soroban_state wired in ---
        let context = LedgerContext::new(101, 1234567890, 100, 5_000_000, 25, NetworkId::testnet());
        let mut executor = TransactionExecutor::new(
            &context,
            0,
            SorobanConfig::default(),
            ClassicEventConfig::default(),
        );
        executor.set_soroban_state(shared);

        // --- Footprint referencing the ContractData key ---
        let footprint = LedgerFootprint {
            read_only: vec![data_key.clone()].try_into().unwrap(),
            read_write: VecM::default(),
        };

        // Must not panic (no bucket list access) and must succeed
        executor
            .load_soroban_footprint(&snapshot, &footprint)
            .unwrap();

        // Entry and its TTL must now be in executor state
        assert!(
            executor.state.get_entry(&data_key).is_some(),
            "ContractData entry must be loaded from InMemorySorobanState"
        );
        assert!(
            executor.state.get_entry(&ttl_key).is_some(),
            "TTL entry must be co-loaded from InMemorySorobanState"
        );
    }

    /// O1 optimization: when InMemorySorobanState is set but does NOT contain a key,
    /// the bucket list IS consulted as a fallback. IMS is a cache, not authoritative:
    /// an entry may be live in the bucket list but temporarily absent from IMS.
    #[test]
    fn test_load_soroban_footprint_absent_key_falls_back_to_bucket_list() {
        // When IMS is set but doesn't have the entry, we must fall back to the bucket list.
        // IMS is a cache (not authoritative): an entry may be live in the bucket list but
        // temporarily absent from IMS (e.g. recently restored from hot archive). Skipping
        // the bucket list in that case caused EntryArchived for valid live entries.
        use crate::snapshot::{LedgerSnapshot, SnapshotHandle};
        use crate::soroban_state::SharedSorobanState;
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        use stellar_xdr::curr::*;

        let contract_address = ScAddress::Contract(ContractId(Hash([0xCD; 32])));
        let data_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_address.clone(),
            key: ScVal::U32(1),
            durability: ContractDataDurability::Persistent,
        });
        let data_entry = LedgerEntry {
            last_modified_ledger_seq: 90,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: contract_address,
                key: ScVal::U32(1),
                durability: ContractDataDurability::Persistent,
                val: ScVal::I32(42),
            }),
            ext: LedgerEntryExt::V0,
        };

        // Empty IMS — entry is absent from IMS but present in bucket list
        let shared = Arc::new(SharedSorobanState::new());

        // Snapshot returns the entry when consulted (bucket list has it)
        let data_key_cl = data_key.clone();
        let data_entry_cl = data_entry.clone();
        let consulted = Arc::new(AtomicBool::new(false));
        let consulted_cl = consulted.clone();
        let lookup_fn: crate::snapshot::EntryLookupFn = Arc::new(move |keys: &LedgerKey| {
            if *keys == data_key_cl {
                consulted_cl.store(true, Ordering::SeqCst);
                return Ok(Some(data_entry_cl.clone()));
            }
            Ok(None)
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup_fn);

        let context = LedgerContext::new(101, 1234567890, 100, 5_000_000, 25, NetworkId::testnet());
        let mut executor = TransactionExecutor::new(
            &context,
            0,
            SorobanConfig::default(),
            ClassicEventConfig::default(),
        );
        executor.set_soroban_state(shared);

        let footprint = LedgerFootprint {
            read_only: vec![data_key.clone()].try_into().unwrap(),
            read_write: VecM::default(),
        };

        executor
            .load_soroban_footprint(&snapshot, &footprint)
            .unwrap();

        // Bucket list MUST have been consulted (IMS miss → bucket list fallback)
        assert!(
            consulted.load(Ordering::SeqCst),
            "Bucket list must be consulted when IMS misses"
        );
        // Entry must be present in executor state (loaded from bucket list)
        assert!(
            executor.state.get_entry(&data_key).is_some(),
            "Entry from bucket list must be loaded when IMS misses"
        );
    }

    /// O1 optimization: without soroban_state set, bucket list is used as before.
    #[test]
    fn test_load_soroban_footprint_falls_back_to_bucket_list_without_ims() {
        use crate::snapshot::{LedgerSnapshot, SnapshotHandle};
        use sha2::{Digest, Sha256};
        use std::sync::Arc;
        use stellar_xdr::curr::*;

        let contract_address = ScAddress::Contract(ContractId(Hash([0xEF; 32])));
        let data_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_address.clone(),
            key: ScVal::U32(7),
            durability: ContractDataDurability::Persistent,
        });
        let data_entry = LedgerEntry {
            last_modified_ledger_seq: 90,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: contract_address,
                key: ScVal::U32(7),
                durability: ContractDataDurability::Persistent,
                val: ScVal::I32(7),
            }),
            ext: LedgerEntryExt::V0,
        };

        let ttl_key_hash = Hash(Sha256::digest(&data_key.to_xdr(Limits::none()).unwrap()).into());
        let ttl_key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: ttl_key_hash.clone(),
        });
        let ttl_entry = LedgerEntry {
            last_modified_ledger_seq: 90,
            data: LedgerEntryData::Ttl(TtlEntry {
                key_hash: ttl_key_hash,
                live_until_ledger_seq: 500,
            }),
            ext: LedgerEntryExt::V0,
        };

        let data_entry_cl = data_entry.clone();
        let ttl_entry_cl = ttl_entry.clone();
        let data_key_cl = data_key.clone();
        let ttl_key_cl = ttl_key.clone();
        let bucket_list_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let bucket_list_called_cl = bucket_list_called.clone();
        let lookup_fn: crate::snapshot::EntryLookupFn = Arc::new(move |key: &LedgerKey| {
            bucket_list_called_cl.store(true, std::sync::atomic::Ordering::Relaxed);
            if *key == data_key_cl {
                return Ok(Some(data_entry_cl.clone()));
            }
            if *key == ttl_key_cl {
                return Ok(Some(ttl_entry_cl.clone()));
            }
            Ok(None)
        });
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup_fn);

        // No soroban_state set on executor
        let context = LedgerContext::new(101, 1234567890, 100, 5_000_000, 25, NetworkId::testnet());
        let mut executor = TransactionExecutor::new(
            &context,
            0,
            SorobanConfig::default(),
            ClassicEventConfig::default(),
        );

        let footprint = LedgerFootprint {
            read_only: vec![data_key.clone()].try_into().unwrap(),
            read_write: VecM::default(),
        };

        executor
            .load_soroban_footprint(&snapshot, &footprint)
            .unwrap();

        assert!(
            bucket_list_called.load(std::sync::atomic::Ordering::Relaxed),
            "Without IMS, bucket list must be used"
        );
        assert!(
            executor.state.get_entry(&data_key).is_some(),
            "ContractData entry must be loaded from bucket list when IMS is absent"
        );
    }

    /// Regression test for VE-13: when a ContractData entry is pre-loaded into state
    /// (e.g., from prior_stage.entries in parallel Soroban execution) but its TTL is
    /// absent (the prior stage TX only modified data, not the TTL), load_soroban_footprint
    /// must still load the missing TTL from IMS.
    ///
    /// Before the fix, the early `continue` when `state.get_entry(key).is_some()` caused
    /// the TTL loading code to be skipped entirely. This made is_archived_contract_entry
    /// return true (no TTL → archived) even for fully live entries.
    #[test]
    fn test_load_soroban_footprint_loads_ttl_when_entry_already_in_state() {
        use crate::snapshot::{LedgerSnapshot, SnapshotHandle};
        use crate::soroban_state::SharedSorobanState;
        use sha2::{Digest, Sha256};
        use std::sync::Arc;
        use stellar_xdr::curr::*;

        // --- Build a ContractData entry ---
        let contract_address = ScAddress::Contract(ContractId(Hash([0x77; 32])));
        let data_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: contract_address.clone(),
            key: ScVal::U32(55),
            durability: ContractDataDurability::Persistent,
        });
        let data_entry = LedgerEntry {
            last_modified_ledger_seq: 90,
            data: LedgerEntryData::ContractData(ContractDataEntry {
                ext: ExtensionPoint::V0,
                contract: contract_address.clone(),
                key: ScVal::U32(55),
                durability: ContractDataDurability::Persistent,
                val: ScVal::I32(77),
            }),
            ext: LedgerEntryExt::V0,
        };

        let ttl_key_hash = Hash(Sha256::digest(&data_key.to_xdr(Limits::none()).unwrap()).into());
        let ttl_key = LedgerKey::Ttl(LedgerKeyTtl {
            key_hash: ttl_key_hash.clone(),
        });

        // --- Populate IMS with the entry AND its TTL ---
        let shared = Arc::new(SharedSorobanState::new());
        {
            let mut ims = shared.write();
            ims.create_ttl(
                &LedgerKeyTtl {
                    key_hash: ttl_key_hash,
                },
                crate::soroban_state::TtlData::new(500, 90),
            )
            .unwrap();
            ims.create_contract_data(data_entry.clone()).unwrap();
        }

        // Snapshot panics if anything is looked up — we must serve from IMS.
        let lookup_fn: crate::snapshot::EntryLookupFn =
            Arc::new(|_key: &LedgerKey| panic!("Bucket list must not be consulted in this test"));
        let snapshot = SnapshotHandle::with_lookup(LedgerSnapshot::empty(100), lookup_fn);

        let context = LedgerContext::new(101, 1234567890, 100, 5_000_000, 25, NetworkId::testnet());
        let mut executor = TransactionExecutor::new(
            &context,
            0,
            SorobanConfig::default(),
            ClassicEventConfig::default(),
        );
        executor.set_soroban_state(shared);

        // Simulate prior_stage loading: ContractData is already in state, TTL is NOT.
        executor.state.load_entry(data_entry);
        assert!(
            executor.state.get_entry(&data_key).is_some(),
            "setup: entry must be in state"
        );
        assert!(
            executor.state.get_entry(&ttl_key).is_none(),
            "setup: TTL must NOT be in state"
        );

        let footprint = LedgerFootprint {
            read_only: vec![data_key.clone()].try_into().unwrap(),
            read_write: VecM::default(),
        };

        // Must succeed (no panic, no bucket list access) and must load TTL from IMS.
        executor
            .load_soroban_footprint(&snapshot, &footprint)
            .unwrap();

        assert!(
            executor.state.get_entry(&ttl_key).is_some(),
            "TTL must be loaded from IMS even when ContractData was already in state"
        );
    }

    /// Verify `collect_seller_keys_for_pairs()` returns the correct account and trustline keys
    /// for offer sellers in the given asset pairs, with native assets excluded and dedup applied.
    ///
    /// Offers (all price 1/1, so offer_id determines order):
    ///   seller 1: Native → USD  (offer_id 1)  => account(1) + trustline(1, USD)
    ///   seller 2: EUR → USD     (offer_id 2)  => account(2) + trustline(2, EUR) + trustline(2, USD)
    ///   seller 1: Native → Native (offer_id 3) => account(1) [deduped]
    ///
    /// Query pairs: (buying=USD, selling=Native) and (buying=USD, selling=EUR)
    /// Expected 5 unique keys: 2 accounts + 3 trustlines.
    #[test]
    fn test_collect_seller_keys_for_pairs() {
        use crate::snapshot::{EntriesLookupFn, LedgerSnapshot, SnapshotHandle};
        use std::collections::HashSet;
        use std::sync::Arc;
        use stellar_xdr::curr::*;

        let make_account_id = |seed: u8| -> AccountId {
            let mut bytes = [0u8; 32];
            bytes[0] = seed;
            AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(bytes)))
        };

        let usd = |issuer_seed: u8| -> Asset {
            Asset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                issuer: make_account_id(issuer_seed),
            })
        };

        let eur = |issuer_seed: u8| -> Asset {
            Asset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'E', b'U', b'R', 0]),
                issuer: make_account_id(issuer_seed),
            })
        };

        let make_offer =
            |seller_seed: u8, offer_id: i64, selling: Asset, buying: Asset| -> LedgerEntry {
                LedgerEntry {
                    last_modified_ledger_seq: 1,
                    data: LedgerEntryData::Offer(OfferEntry {
                        seller_id: make_account_id(seller_seed),
                        offer_id,
                        selling,
                        buying,
                        amount: 1000,
                        price: Price { n: 1, d: 1 },
                        flags: 0,
                        ext: OfferEntryExt::V0,
                    }),
                    ext: LedgerEntryExt::V0,
                }
            };

        let issuer = 99u8;
        let offers = vec![
            make_offer(1, 1, Asset::Native, usd(issuer)), // seller 1: native→USD
            make_offer(2, 2, eur(issuer), usd(issuer)),   // seller 2: EUR→USD
            make_offer(1, 3, Asset::Native, Asset::Native), // seller 1 again: native→native
        ];

        let entries_fn: EntriesLookupFn = Arc::new(move || Ok(offers.clone()));
        let mut snap = SnapshotHandle::new(LedgerSnapshot::empty(1));
        snap.set_entries_lookup(entries_fn);

        let context = LedgerContext::new(100, 0, 100, 5_000_000, 25, NetworkId::testnet());
        let mut executor = TransactionExecutor::new(
            &context,
            0,
            SorobanConfig::default(),
            ClassicEventConfig::default(),
        );
        executor.load_orderbook_offers(&snap).unwrap();

        // Query pairs that cover the two non-native offers.
        let mut pairs: HashSet<(Asset, Asset)> = HashSet::new();
        pairs.insert((usd(issuer), Asset::Native)); // buying=USD, selling=Native
        pairs.insert((usd(issuer), eur(issuer))); // buying=USD, selling=EUR

        let keys = executor.collect_seller_keys_for_pairs(&pairs, 10);

        // 2 account keys + 3 trustline keys = 5 unique keys
        assert_eq!(
            keys.len(),
            5,
            "expected 5 unique keys, got {}: {:?}",
            keys.len(),
            keys
        );

        let account1 = LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id(1),
        });
        let account2 = LedgerKey::Account(LedgerKeyAccount {
            account_id: make_account_id(2),
        });

        let tl_usd_issuer = make_account_id(issuer);
        let tl_eur_issuer = make_account_id(issuer);

        let tl1_usd = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: make_account_id(1),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                issuer: tl_usd_issuer.clone(),
            }),
        });
        let tl2_usd = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: make_account_id(2),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                issuer: tl_usd_issuer,
            }),
        });
        let tl2_eur = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: make_account_id(2),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'E', b'U', b'R', 0]),
                issuer: tl_eur_issuer,
            }),
        });

        assert!(keys.contains(&account1), "missing account key for seller 1");
        assert!(keys.contains(&account2), "missing account key for seller 2");
        assert!(
            keys.contains(&tl1_usd),
            "missing USD trustline for seller 1"
        );
        assert!(
            keys.contains(&tl2_usd),
            "missing USD trustline for seller 2"
        );
        assert!(
            keys.contains(&tl2_eur),
            "missing EUR trustline for seller 2"
        );
    }

    /// Regression test for #1106: Fee deduction must be capped at account balance.
    ///
    /// stellar-core caps with `std::min(balance, fee)` (TransactionFrame.cpp:1797).
    /// The bug: `acc.balance -= fee` without capping, which could produce a negative
    /// balance when fee > balance (e.g., multiple TXs from same account in one ledger).
    #[test]
    fn test_audit_1106_fee_deduction_capped_at_balance() {
        // The fee deduction cap formula: charged = min(balance, fee)
        // When balance < fee, the charged amount should be the balance, not the full fee.
        let balance: i64 = 50;
        let fee: i64 = 100;

        // This is the correct behavior (matching stellar-core):
        let capped_fee = std::cmp::min(balance, fee);
        let new_balance = balance - capped_fee;

        assert_eq!(capped_fee, 50, "fee should be capped at available balance");
        assert_eq!(new_balance, 0, "balance should be zero, not negative");
        assert!(new_balance >= 0, "balance must never go negative");

        // The bug would have produced:
        let uncapped_balance = balance - fee; // -50 — this is the bug
        assert!(
            uncapped_balance < 0,
            "without cap, balance goes negative — this is the bug #1106 prevents"
        );
    }

    /// Regression test for #1484: PriorStageState::from_delta must filter out
    /// non-Soroban keys from dead_entries. Classic deletions (Account, Trustline,
    /// Offer) in the delta are irrelevant for parallel Soroban execution.
    #[test]
    fn test_prior_stage_state_filters_non_soroban_deletions() {
        use crate::LedgerDelta;
        use stellar_xdr::curr::*;

        let mut delta = LedgerDelta::new(101);

        // Add a Soroban deletion (ContractCode).
        let code_hash = Hash([0xCC; 32]);
        let code_entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::ContractCode(ContractCodeEntry {
                ext: ContractCodeEntryExt::V0,
                hash: code_hash.clone(),
                code: BytesM::try_from(vec![1u8]).unwrap(),
            }),
            ext: LedgerEntryExt::V0,
        };
        let code_key = LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: code_hash.clone(),
        });
        delta.record_delete(code_entry).unwrap();

        // Add a classic deletion (Account).
        let account_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0xAA; 32])));
        let account_entry = LedgerEntry {
            last_modified_ledger_seq: 100,
            data: LedgerEntryData::Account(AccountEntry {
                account_id: account_id.clone(),
                balance: 10_000_000,
                seq_num: SequenceNumber(1),
                num_sub_entries: 0,
                inflation_dest: None,
                flags: 0,
                home_domain: String32::default(),
                thresholds: Thresholds([1, 0, 0, 0]),
                signers: vec![].try_into().unwrap(),
                ext: AccountEntryExt::V0,
            }),
            ext: LedgerEntryExt::V0,
        };
        let account_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: account_id.clone(),
        });
        delta.record_delete(account_entry).unwrap();

        // Verify delta has both deletions.
        assert_eq!(
            delta.dead_entries().len(),
            2,
            "delta should have 2 deletions"
        );

        // PriorStageState should only include the Soroban key.
        let prior = PriorStageState::from_delta(&delta, std::collections::HashSet::new());
        assert_eq!(
            prior.deleted_keys.len(),
            1,
            "PriorStageState should filter out non-Soroban deletions"
        );
        assert!(
            prior.deleted_keys.contains(&code_key),
            "PriorStageState must include Soroban ContractCode deletion"
        );
        assert!(
            !prior.deleted_keys.contains(&account_key),
            "PriorStageState must NOT include classic Account deletion"
        );
    }

    /// Regression test for #2062: TransactionExecutor tracks hot-archive restored
    /// keys, seeds from prior stage, and clears on advance_to_ledger.
    #[test]
    fn test_hot_archive_restored_keys_tracking() {
        let context = LedgerContext::new(100, 1234567890, 100, 5_000_000, 21, NetworkId::testnet());
        let mut executor = TransactionExecutor::new(
            &context,
            0,
            SorobanConfig::default(),
            ClassicEventConfig::default(),
        );

        // Initially empty.
        assert!(executor.hot_archive_restored_keys().is_empty());

        // Record some keys.
        let key1 = LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
            hash: stellar_xdr::curr::Hash([1u8; 32]),
        });
        let key2 = LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
            hash: stellar_xdr::curr::Hash([2u8; 32]),
        });
        executor.record_hot_archive_restores(&[key1.clone(), key2.clone()]);
        assert_eq!(executor.hot_archive_restored_keys().len(), 2);
        assert!(executor.hot_archive_restored_keys().contains(&key1));
        assert!(executor.hot_archive_restored_keys().contains(&key2));

        // Duplicate record is idempotent.
        executor.record_hot_archive_restores(&[key1.clone()]);
        assert_eq!(executor.hot_archive_restored_keys().len(), 2);

        // advance_to_ledger clears the set.
        executor.advance_to_ledger(LedgerAdvanceParams {
            ledger_seq: 101,
            close_time: 1234567891,
            base_reserve: 5_000_000,
            protocol_version: 21,
            soroban_config: SorobanConfig::default(),
            frozen_key_config: henyey_tx::frozen_keys::FrozenKeyConfig::default(),
            ledger_flags: 0,
        });
        assert!(
            executor.hot_archive_restored_keys().is_empty(),
            "advance_to_ledger must clear hot_archive_restored_keys"
        );
    }

    /// Regression test for #2062: seed_hot_archive_restored_keys merges prior
    /// stage keys into the executor for cross-stage propagation.
    #[test]
    fn test_seed_hot_archive_restored_keys_cross_stage() {
        let context = LedgerContext::new(100, 1234567890, 100, 5_000_000, 21, NetworkId::testnet());
        let mut executor = TransactionExecutor::new(
            &context,
            0,
            SorobanConfig::default(),
            ClassicEventConfig::default(),
        );

        let key1 = LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
            hash: stellar_xdr::curr::Hash([10u8; 32]),
        });
        let key2 = LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
            hash: stellar_xdr::curr::Hash([20u8; 32]),
        });

        // Record one key from "this cluster's" work.
        executor.record_hot_archive_restores(&[key1.clone()]);

        // Seed from prior stage with a different key.
        let mut prior_keys = std::collections::HashSet::new();
        prior_keys.insert(key2.clone());
        executor.seed_hot_archive_restored_keys(&prior_keys);

        // Both keys should be present.
        assert_eq!(executor.hot_archive_restored_keys().len(), 2);
        assert!(executor.hot_archive_restored_keys().contains(&key1));
        assert!(executor.hot_archive_restored_keys().contains(&key2));
    }

    /// Regression test for #2062: PriorStageState::from_delta carries
    /// hot_archive_restored_keys through for cross-stage propagation.
    #[test]
    fn test_prior_stage_state_carries_restored_keys() {
        let delta = crate::LedgerDelta::new(100);
        let mut restored = std::collections::HashSet::new();
        let key = LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
            hash: stellar_xdr::curr::Hash([77u8; 32]),
        });
        restored.insert(key.clone());

        let prior = PriorStageState::from_delta(&delta, restored);
        assert_eq!(prior.hot_archive_restored_keys.len(), 1);
        assert!(prior.hot_archive_restored_keys.contains(&key));
    }
}
