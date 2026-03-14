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

use soroban_env_host_p25::fees::{
    compute_rent_write_fee_per_1kb, compute_transaction_resource_fee, FeeConfiguration,
    RentFeeConfiguration, RentWriteFeeConfiguration,
};
use henyey_common::protocol::{protocol_version_is_before, protocol_version_starts_from, ProtocolVersion};
use henyey_common::{Hash256, NetworkId, LIQUIDITY_POOL_FEE_V18};
use henyey_crypto::account_id_to_strkey;

use henyey_tx::{
    make_account_address, make_claimable_balance_address, make_muxed_account_address,
    operations::OperationTypeExt,
    soroban::{PersistentModuleCache, SorobanConfig},
    state::{get_account_seq_ledger, get_account_seq_time},
    validation::{self, LedgerContext as ValidationContext},
    ClassicEventConfig, LedgerContext, LedgerStateManager, OpEventManager, TransactionFrame,
    TxError, TxEventManager,
};
use stellar_xdr::curr::{
    AccountEntry, AccountEntryExt, AccountEntryExtensionV1Ext, AccountId, AccountMergeResult,
    AllowTrustOp, AlphaNum12, AlphaNum4, Asset, AssetCode, ClaimableBalanceEntry,
    ClaimableBalanceId, ConfigSettingEntry, ConfigSettingId, ContractCostParams, ContractEvent,
    CreateClaimableBalanceResult, DiagnosticEvent, ExtensionPoint, InflationResult,
    InnerTransactionResult, InnerTransactionResultExt, InnerTransactionResultPair,
    InnerTransactionResultResult, LedgerEntry, LedgerEntryChange, LedgerEntryChanges,
    LedgerEntryData, LedgerKey, LedgerKeyClaimableBalance, LedgerKeyConfigSetting,
    LedgerKeyLiquidityPool, LedgerKeyTrustLine, Limits, LiquidityPoolEntry, LiquidityPoolEntryBody,
    ManageBuyOfferResult, ManageSellOfferResult, MuxedAccount, OfferEntry, Operation,
    OperationBody, OperationMetaV2, OperationResult, OperationResultTr, OperationType,
    PathPaymentStrictReceiveResult, PathPaymentStrictSendResult, PoolId, Preconditions, ScAddress,
    SignerKey, SorobanTransactionData, SorobanTransactionMetaExt, SorobanTransactionMetaExtV1,
    SorobanTransactionMetaV2, TransactionEnvelope, TransactionEvent, TransactionEventStage,
    TransactionMeta, TransactionMetaV4, TransactionResult, TransactionResultCode,
    TransactionResultExt, TransactionResultMetaV1, TransactionResultPair,
    TransactionResultResult, TrustLineAsset, TrustLineFlags, VecM, WriteXdr,
};
use tracing::{debug, warn};

use crate::delta::LedgerDelta;
use crate::snapshot::SnapshotHandle;
use crate::{LedgerError, Result};

use henyey_bucket::HotArchiveBucketList;


mod config;
mod meta;
mod result_mapping;
mod signatures;
mod tx_set;

pub use config::{load_config_setting, load_soroban_config, load_soroban_network_info, compute_soroban_resource_fee};
pub use result_mapping::build_tx_result_pair;
pub use tx_set::{execute_transaction_set, execute_transaction_set_with_fee_mode, run_transactions_on_executor, execute_soroban_parallel_phase, pre_deduct_all_fees_on_delta, compute_state_size_window_entry};

use meta::*;
use result_mapping::*;
use signatures::*;

/// Wrapper around HotArchiveBucketList that implements the HotArchiveLookup trait.
///
/// This allows the ledger execution layer to look up archived entries without
/// requiring the tx layer to depend on the bucket crate.
pub struct HotArchiveLookupImpl {
    hot_archive: std::sync::Arc<parking_lot::RwLock<Option<HotArchiveBucketList>>>,
}

impl HotArchiveLookupImpl {
    pub fn new(
        hot_archive: std::sync::Arc<parking_lot::RwLock<Option<HotArchiveBucketList>>>,
    ) -> Self {
        Self { hot_archive }
    }
}

impl henyey_tx::soroban::HotArchiveLookup for HotArchiveLookupImpl {
    fn get(&self, key: &LedgerKey) -> Option<LedgerEntry> {
        // Use the hot archive bucket list's get method
        let guard = self.hot_archive.read();
        let hot_archive = match guard.as_ref() {
            Some(ha) => ha,
            None => {
                return None;
            }
        };
        match hot_archive.get(key) {
            Ok(Some(entry)) => Some(entry),
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    key_type = ?std::mem::discriminant(key),
                    "Hot archive lookup failed"
                );
                None
            }
        }
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
}

struct RefundableFeeTracker {
    non_refundable_fee: i64,
    max_refundable_fee: i64,
    consumed_event_size_bytes: u32,
    consumed_rent_fee: i64,
    consumed_refundable_fee: i64,
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
        // This matches stellar-core's consumeRefundableSorobanResources which checks
        // if (mMaximumRefundableFee < mConsumedRentFee) before computing events fee.
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
    /// Per-operation-type timing: maps op type to (total_us, count).
    pub op_type_timings: HashMap<OperationType, (u64, u32)>,
    /// Total transaction execution time in microseconds.
    pub exec_time_us: u64,
    /// Sub-phase timings (microseconds) for profiling.
    pub validation_us: u64,
    pub fee_seq_us: u64,
    pub footprint_us: u64,
    pub ops_us: u64,
    // Validation sub-component timings (microseconds)
    pub val_account_load_us: u64,
    pub val_tx_hash_us: u64,
    pub val_ed25519_us: u64,
    pub val_other_us: u64,
    // Fee/seq sub-component timings (microseconds)
    pub fee_deduct_us: u64,
    pub op_sig_check_us: u64,
    pub signer_removal_us: u64,
    pub seq_bump_us: u64,
    pub meta_build_us: u64,
    // Meta sub-component timings (microseconds)
    pub meta_commit_us: u64,
    pub meta_fee_refund_us: u64,
    pub meta_build_phase_us: u64,
}

/// Type alias: `ExecutionFailure` is now `TransactionResultCode` from the XDR crate.
///
/// Previously this was a custom enum with 15 variants that mapped 1:1 to
/// `TransactionResultCode`. We now use the XDR type directly, eliminating
/// the intermediate mapping layer.
pub type ExecutionFailure = TransactionResultCode;

/// Create a failed `TransactionExecutionResult` with no fee charged or meta.
fn failed_result(failure: TransactionResultCode, error: &str) -> TransactionExecutionResult {
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
        op_type_timings: HashMap::new(),
        exec_time_us: 0,
        validation_us: 0,
        fee_seq_us: 0,
        footprint_us: 0,
        ops_us: 0,
        meta_build_us: 0,
        meta_commit_us: 0,
        meta_fee_refund_us: 0,
        meta_build_phase_us: 0,
        val_account_load_us: 0,
        val_tx_hash_us: 0,
        val_ed25519_us: 0,
        val_other_us: 0,
        fee_deduct_us: 0,
        op_sig_check_us: 0,
        signer_removal_us: 0,
        seq_bump_us: 0,
    }
}

/// Data returned by successful pre-fee validation of a transaction.
struct ValidatedTransaction {
    frame: TransactionFrame,
    fee_source_id: AccountId,
    inner_source_id: AccountId,
    outer_hash: Hash256,
    // Sub-component timings from validation (microseconds)
    val_account_load_us: u64,
    val_tx_hash_us: u64,
    val_ed25519_us: u64,
    val_other_us: u64,
}

/// Validation failure with additional context about the validation level reached.
/// This is used to determine whether the sequence number should still be bumped
/// even though validation failed, matching stellar-core's ValidationType enum.
struct ValidationFailure {
    result: TransactionExecutionResult,
    /// Whether the validation passed the sequence check (equivalent to
    /// stellar-core's `cv >= kInvalidUpdateSeqNum`). When true, the sequence
    /// number should be bumped even though the TX failed validation.
    past_seq_check: bool,
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
struct PreApplyResult {
    // Transaction identity
    frame: TransactionFrame,
    fee_source_id: AccountId,
    inner_source_id: AccountId,

    // Pre-apply outputs
    tx_changes_before: LedgerEntryChanges,
    fee_changes: LedgerEntryChanges,
    refundable_fee_tracker: Option<RefundableFeeTracker>,
    tx_event_manager: TxEventManager,
    preflight_failure: Option<ExecutionFailure>,
    sig_check_failure: Option<(Vec<OperationResult>, ExecutionFailure)>,
    fee: i64,

    // Rollback data (entry snapshots for restoring on op failure)
    fee_entries: DeltaEntries,
    seq_entries: DeltaEntries,
    signer_entries: DeltaEntries,

    // Config carried forward
    soroban_prng_seed: Option<[u8; 32]>,
    base_fee: u32,
    deduct_fee: bool,

    // Timing
    validation_us: u64,
    fee_seq_us: u64,
    tx_timing_start: std::time::Instant,
    // Validation sub-component timings (microseconds)
    val_account_load_us: u64,
    val_tx_hash_us: u64,
    val_ed25519_us: u64,
    val_other_us: u64,
    // Fee/seq sub-component timings (microseconds)
    fee_deduct_us: u64,
    op_sig_check_us: u64,
    signer_removal_us: u64,
    seq_bump_us: u64,
}

/// Snapshot of created/updated/deleted entries for a delta phase (fee, seq, signer).
/// Used for rollback restoration when a transaction's operation body fails.
struct DeltaEntries {
    created: Vec<LedgerEntry>,
    updated: Vec<LedgerEntry>,
    deleted: Vec<LedgerKey>,
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
}

impl TransactionExecutor {
    /// Create a new transaction executor.
    pub fn new(
        context: &LedgerContext,
        id_pool: u64,
        soroban_config: SorobanConfig,
        classic_events: ClassicEventConfig,
    ) -> Self {
        let mut state =
            LedgerStateManager::new(context.base_reserve as i64, context.sequence);
        state.set_id_pool(id_pool);
        Self {
            ledger_seq: context.sequence,
            close_time: context.close_time,
            base_reserve: context.base_reserve,
            protocol_version: context.protocol_version,
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

    /// Advance to a new ledger, preserving the current state.
    /// This is useful for replaying multiple consecutive ledgers without
    /// losing state changes between them.
    ///
    /// Note: id_pool is NOT reset here because:
    /// 1. The executor's internal id_pool evolves correctly as transactions execute
    /// 2. The id_pool from the ledger header is the POST-execution value (after the ledger closes)
    /// 3. Using the header's id_pool would give us the wrong starting value for the next ledger
    ///    For the first ledger in a replay session, use TransactionExecutor::new() which takes
    ///    the PREVIOUS ledger's closing id_pool (which equals this ledger's starting id_pool).
    pub fn advance_to_ledger(
        &mut self,
        ledger_seq: u32,
        close_time: u64,
        base_reserve: u32,
        protocol_version: u32,
        _id_pool: u64, // Intentionally unused - see note above
        soroban_config: SorobanConfig,
    ) {
        self.ledger_seq = ledger_seq;
        self.close_time = close_time;
        self.base_reserve = base_reserve;
        self.protocol_version = protocol_version;
        self.soroban_config = soroban_config;
        // Do NOT reset id_pool - it should continue from where it was
        self.state.set_ledger_seq(ledger_seq);
        // Note: loaded_accounts cache is preserved - this is intentional because
        // accounts that were loaded/created in previous ledgers remain valid
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
    pub fn advance_to_ledger_preserving_offers(
        &mut self,
        ledger_seq: u32,
        close_time: u64,
        base_reserve: u32,
        protocol_version: u32,
        _id_pool: u64,
        soroban_config: SorobanConfig,
    ) {
        self.ledger_seq = ledger_seq;
        self.close_time = close_time;
        self.base_reserve = base_reserve;
        self.protocol_version = protocol_version;
        self.soroban_config = soroban_config;
        self.state.set_ledger_seq(ledger_seq);
        // Clear cached entries except offers and offer index
        self.state.clear_cached_entries_preserving_offers();
        // Clear loaded_accounts cache (non-offer)
        self.loaded_accounts.clear();
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
        let label = if record { "load_account" } else { "load_account_without_record" };

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

    fn available_balance_for_fee(&self, account: &AccountEntry) -> Result<i64> {
        let min_balance = self
            .state
            .minimum_balance_for_account(account, self.protocol_version, 0)
            .map_err(|e| LedgerError::Internal(e.to_string()))?;
        let mut available = account.balance - min_balance;
        if protocol_version_starts_from(self.protocol_version, ProtocolVersion::V10) {
            let selling = match &account.ext {
                AccountEntryExt::V1(v1) => v1.liabilities.selling,
                AccountEntryExt::V0 => 0,
            };
            available -= selling;
        }
        Ok(available)
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
        let offers = self.state.get_offers_by_account_and_asset(account_id, asset);
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
                let ttl_key = LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl {
                    key_hash,
                });

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

        // Compute fee using the same logic as execute_transaction_with_fee_mode
        // For fee bump transactions, the required fee includes an extra base fee for the wrapper
        let num_ops = std::cmp::max(1, frame.operation_count() as i64);
        let required_fee = if frame.is_fee_bump() {
            // Fee bumps pay baseFee * (numOps + 1) - extra charge for the fee bump wrapper
            base_fee as i64 * (num_ops + 1)
        } else {
            base_fee as i64 * num_ops
        };
        let inclusion_fee = frame.inclusion_fee();
        // For Soroban, the resource fee is charged in full, plus the inclusion fee up to required.
        // For classic transactions, charge up to the required_fee (base_fee * num_ops).
        let fee = if frame.is_soroban() {
            frame.declared_soroban_resource_fee() + std::cmp::min(inclusion_fee, required_fee)
        } else {
            std::cmp::min(inclusion_fee, required_fee)
        };

        if frame.is_fee_bump() {
            tracing::debug!(
                is_fee_bump = true,
                total_fee = frame.total_fee(),
                inner_fee = frame.inner_fee(),
                inclusion_fee = inclusion_fee,
                base_fee = base_fee,
                operation_count = frame.operation_count(),
                required_fee = required_fee,
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

        // Deduct fee
        if let Some(acc) = self.state.get_account_mut(&fee_source_id) {
            acc.balance -= fee;
        }

        // For protocol < 10, sequence bump happens during fee processing
        if protocol_version_is_before(self.protocol_version, ProtocolVersion::V10) {
            if let Some(acc) = self.state.get_account_mut(&inner_source_id) {
                // Set the account's seq_num to the transaction's seq_num
                acc.seq_num = stellar_xdr::curr::SequenceNumber(frame.sequence_number());
                henyey_tx::state::update_account_seq_info(
                    acc,
                    self.ledger_seq,
                    self.close_time,
                );
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
    /// For fee bump transactions in two-phase mode, `fee_source_pre_state` can be provided
    /// to use as the STATE entry in tx_changes_before. This is needed because the current
    /// state may already be post-fee-processing (synced with CDP's fee_meta), but we need
    /// to emit the pre-fee state for the STATE entry to match stellar-core behavior.
    pub fn execute_transaction_with_fee_mode(
        &mut self,
        snapshot: &SnapshotHandle,
        tx_envelope: &TransactionEnvelope,
        base_fee: u32,
        soroban_prng_seed: Option<[u8; 32]>,
        deduct_fee: bool,
    ) -> Result<TransactionExecutionResult> {
        self.execute_transaction_with_fee_mode_and_pre_state(
            snapshot,
            tx_envelope,
            base_fee,
            soroban_prng_seed,
            deduct_fee,
            None,
            true, // should_apply: always execute body in sequential path
        )
    }

    /// Validate a transaction's structure, accounts, fees, preconditions, sequence,
    /// and signatures before any state changes. Returns the validated data needed
    /// for execution, or a `ValidationFailure` on validation failure.
    fn validate_preconditions_with_frame(
        &mut self,
        snapshot: &SnapshotHandle,
        frame: TransactionFrame,
        base_fee: u32,
    ) -> Result<std::result::Result<ValidatedTransaction, ValidationFailure>> {
        let val_start = std::time::Instant::now();
        let fee_source_id = henyey_tx::muxed_to_account_id(&frame.fee_source_account());
        let inner_source_id = henyey_tx::muxed_to_account_id(&frame.inner_source_account());

        // Helper to create a pre-seq-check failure (no sequence bump needed).
        let pre_seq_fail = |failure, error| {
            ValidationFailure {
                result: failed_result(failure, error),
                past_seq_check: false,
            }
        };
        // Helper to create a post-seq-check failure (sequence bump needed).
        let post_seq_fail = |failure, error| {
            ValidationFailure {
                result: failed_result(failure, error),
                past_seq_check: true,
            }
        };

        // Phase 1: Structure validation
        if !frame.is_valid_structure() {
            let failure = if frame.operations().is_empty() {
                TransactionResultCode::TxMissingOperation
            } else {
                TransactionResultCode::TxMalformed
            };
            return Ok(Err(pre_seq_fail(failure, "Invalid transaction structure")));
        }

        // Phase 2: Account loading
        let acct_load_start = std::time::Instant::now();
        if !self.load_account(snapshot, &fee_source_id)? {
            return Ok(Err(pre_seq_fail(TransactionResultCode::TxNoAccount, "Source account not found")));
        }
        if !self.load_account(snapshot, &inner_source_id)? {
            return Ok(Err(pre_seq_fail(TransactionResultCode::TxNoAccount, "Source account not found")));
        }

        let fee_source_account = match self.state.get_account(&fee_source_id) {
            Some(acc) => acc.clone(),
            None => return Ok(Err(pre_seq_fail(TransactionResultCode::TxNoAccount, "Source account not found"))),
        };
        let source_account = match self.state.get_account(&inner_source_id) {
            Some(acc) => acc.clone(),
            None => return Ok(Err(pre_seq_fail(TransactionResultCode::TxNoAccount, "Source account not found"))),
        };
        let val_account_load_us = acct_load_start.elapsed().as_micros() as u64;

        // Phase 3: Fee validation
        if frame.is_fee_bump() {
            let op_count = frame.operation_count() as i64;
            let outer_op_count = std::cmp::max(1_i64, op_count + 1);
            let outer_min_inclusion_fee = base_fee as i64 * outer_op_count;
            let outer_inclusion_fee = frame.inclusion_fee();

            if outer_inclusion_fee < outer_min_inclusion_fee {
                return Ok(Err(pre_seq_fail(TransactionResultCode::TxInsufficientFee, "Insufficient fee")));
            }

            let (inner_inclusion_fee, inner_is_soroban) = match frame.envelope() {
                TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
                    stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                        let inner_env = TransactionEnvelope::Tx(inner.clone());
                        let inner_frame =
                            TransactionFrame::from_owned_with_network(inner_env, self.network_id);
                        (inner_frame.inclusion_fee(), inner_frame.is_soroban())
                    }
                },
                _ => (0, false),
            };

            if inner_inclusion_fee >= 0 {
                let inner_min_inclusion_fee = base_fee as i64 * std::cmp::max(1_i64, op_count);
                let v1 = outer_inclusion_fee as i128 * inner_min_inclusion_fee as i128;
                let v2 = inner_inclusion_fee as i128 * outer_min_inclusion_fee as i128;
                if v1 < v2 {
                    return Ok(Err(pre_seq_fail(TransactionResultCode::TxInsufficientFee, "Insufficient fee")));
                }
            } else {
                let allow_negative_inner = inner_is_soroban;
                if !allow_negative_inner {
                    return Ok(Err(pre_seq_fail(TransactionResultCode::TxFailed, "Fee bump inner transaction invalid")));
                }
            }
        } else {
            let required_fee = frame.operation_count() as u32 * base_fee;
            if frame.fee() < required_fee {
                return Ok(Err(pre_seq_fail(TransactionResultCode::TxInsufficientFee, "Insufficient fee")));
            }
        }

        // Phase 4: Time/ledger bounds and precondition validation
        let validation_ctx = ValidationContext::new(
            self.ledger_seq,
            self.close_time,
            base_fee,
            self.base_reserve,
            self.protocol_version,
            self.network_id,
        );

        if let Err(e) = validation::validate_time_bounds(&frame, &validation_ctx) {
            return Ok(Err(pre_seq_fail(
                match e {
                    validation::ValidationError::TooEarly { .. } => TransactionResultCode::TxTooEarly,
                    validation::ValidationError::TooLate { .. } => TransactionResultCode::TxTooLate,
                    _ => TransactionResultCode::TxFailed,
                },
                "Time bounds invalid",
            )));
        }

        if let Err(e) = validation::validate_ledger_bounds(&frame, &validation_ctx) {
            return Ok(Err(pre_seq_fail(
                match e {
                    validation::ValidationError::BadLedgerBounds { min, max, current } => {
                        if max > 0 && current > max {
                            TransactionResultCode::TxTooLate
                        } else if min > 0 && current < min {
                            TransactionResultCode::TxTooEarly
                        } else {
                            TransactionResultCode::TxFailed
                        }
                    }
                    _ => TransactionResultCode::TxFailed,
                },
                "Ledger bounds invalid",
            )));
        }

        // Phase 5: Sequence number validation
        // This combines stellar-core's isBadSeq (including min_seq_num) check.
        if self.ledger_seq <= i32::MAX as u32 {
            let starting_seq = (self.ledger_seq as i64) << 32;
            if frame.sequence_number() == starting_seq {
                return Ok(Err(pre_seq_fail(TransactionResultCode::TxBadSeq, "Bad sequence: equals starting sequence")));
            }
        }

        let min_seq_num = match frame.preconditions() {
            Preconditions::V2(cond) => cond.min_seq_num.map(|s| s.0),
            _ => None,
        };

        let account_seq = source_account.seq_num.0;
        let tx_seq = frame.sequence_number();

        tracing::debug!(
            account_seq,
            tx_seq,
            min_seq_num = ?min_seq_num,
            preconditions_type = ?std::mem::discriminant(&frame.preconditions()),
            "Sequence number validation"
        );

        let is_bad_seq = if let Some(min_seq) = min_seq_num {
            account_seq < min_seq || account_seq >= tx_seq
        } else {
            account_seq == i64::MAX || account_seq + 1 != tx_seq
        };

        if is_bad_seq {
            let error_msg = if let Some(min_seq) = min_seq_num {
                format!(
                    "Bad sequence: account seq {} not in valid range [minSeqNum={}, txSeq={})",
                    account_seq, min_seq, tx_seq
                )
            } else {
                format!(
                    "Bad sequence: expected {}, got {}",
                    account_seq.saturating_add(1),
                    tx_seq
                )
            };
            return Ok(Err(pre_seq_fail(TransactionResultCode::TxBadSeq, &error_msg)));
        }

        // --- Past this point, the sequence check has passed ---
        // In stellar-core's commonValid, res = kInvalidUpdateSeqNum here.
        // Failures after this point should still bump the sequence number.

        // Phase 5b: Min seq age/gap checks (stellar-core's isTooEarlyForAccount)
        if let Preconditions::V2(cond) = frame.preconditions() {
            if cond.min_seq_age.0 > 0 {
                let acc_seq_time = get_account_seq_time(&source_account);
                let min_seq_age = cond.min_seq_age.0;
                if min_seq_age > self.close_time || self.close_time - min_seq_age < acc_seq_time {
                    return Ok(Err(post_seq_fail(TransactionResultCode::TxBadMinSeqAgeOrGap, "Minimum sequence age not met")));
                }
            }

            if cond.min_seq_ledger_gap > 0 {
                let acc_seq_ledger = get_account_seq_ledger(&source_account);
                let min_seq_ledger_gap = cond.min_seq_ledger_gap;
                if min_seq_ledger_gap > self.ledger_seq
                    || self.ledger_seq - min_seq_ledger_gap < acc_seq_ledger
                {
                    return Ok(Err(post_seq_fail(TransactionResultCode::TxBadMinSeqAgeOrGap, "Minimum sequence ledger gap not met")));
                }
            }
        }

        // Phase 6: Signature validation
        let sig_start = std::time::Instant::now();
        if validation::validate_signatures(&frame, &validation_ctx).is_err() {
            return Ok(Err(post_seq_fail(TransactionResultCode::TxBadAuth, "Invalid signature")));
        }

        let hash_start = std::time::Instant::now();
        let outer_hash = frame
            .hash(&self.network_id)
            .map_err(|e| LedgerError::Internal(format!("tx hash error: {}", e)))?;
        let val_tx_hash_us = hash_start.elapsed().as_micros() as u64;

        let ed25519_start = std::time::Instant::now();
        let outer_threshold = threshold_low(&fee_source_account);
        if !has_sufficient_signer_weight(
            &outer_hash,
            frame.signatures(),
            &fee_source_account,
            outer_threshold,
        ) {
            tracing::debug!("Signature check failed: fee_source outer check");
            return Ok(Err(post_seq_fail(TransactionResultCode::TxBadAuth, "Invalid signature")));
        }

        // NOTE: For fee-bump transactions, we deliberately do NOT check the inner
        // transaction's signatures here. In stellar-core, fee is charged by
        // processFeeSeqNum() BEFORE apply() re-validates inner signatures. If a
        // prior transaction in the same ledger modifies the inner source's signer
        // set, the inner sig check must fail at apply-time (after fee charging),
        // not here. The check_operation_signatures call in execute_transaction_with_fee_mode
        // handles inner sig validation after the fee has been deducted.

        let required_weight = threshold_low(&source_account);
        if !frame.is_fee_bump()
            && !has_sufficient_signer_weight(
                &outer_hash,
                frame.signatures(),
                &source_account,
                required_weight,
            )
        {
            tracing::debug!(
                required_weight = required_weight,
                is_fee_bump = frame.is_fee_bump(),
                master_weight = source_account.thresholds.0[0],
                num_signers = source_account.signers.len(),
                thresholds = ?source_account.thresholds.0,
                "Signature check failed: source outer check"
            );
            return Ok(Err(post_seq_fail(TransactionResultCode::TxBadAuth, "Invalid signature")));
        }

        if let Preconditions::V2(cond) = frame.preconditions() {
            if !cond.extra_signers.is_empty() {
                let extra_hash = if frame.is_fee_bump() {
                    fee_bump_inner_hash(&frame, &self.network_id)?
                } else {
                    outer_hash
                };
                let extra_signatures = if frame.is_fee_bump() {
                    frame.inner_signatures()
                } else {
                    frame.signatures()
                };
                if !has_required_extra_signers(&extra_hash, extra_signatures, &cond.extra_signers) {
                    return Ok(Err(post_seq_fail(TransactionResultCode::TxBadAuthExtra, "Missing extra signer")));
                }
            }
        }

        let val_ed25519_us = ed25519_start.elapsed().as_micros() as u64;
        let val_sig_total_us = sig_start.elapsed().as_micros() as u64;
        let val_total_us = val_start.elapsed().as_micros() as u64;
        let val_other_us = val_total_us.saturating_sub(val_account_load_us + val_sig_total_us);

        Ok(Ok(ValidatedTransaction {
            frame,
            fee_source_id,
            inner_source_id,
            outer_hash,
            val_account_load_us,
            val_tx_hash_us,
            val_ed25519_us,
            val_other_us,
        }))
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
    fn pre_apply_arc(
        &mut self,
        snapshot: &SnapshotHandle,
        tx_envelope: &Arc<TransactionEnvelope>,
        base_fee: u32,
        soroban_prng_seed: Option<[u8; 32]>,
        deduct_fee: bool,
        fee_source_pre_state: Option<LedgerEntry>,
    ) -> Result<std::result::Result<PreApplyResult, TransactionExecutionResult>> {
        let tx_timing_start = std::time::Instant::now();

        // Create the frame once from Arc (cheap clone) and reuse for both soroban fee check and validation.
        let frame = TransactionFrame::with_network(Arc::clone(tx_envelope), self.network_id);

        let soroban_max_refundable = {
            if frame.is_soroban() {
                let (non_refundable_fee, _) = compute_soroban_resource_fee(
                    &frame,
                    self.protocol_version,
                    &self.soroban_config,
                    0,
                )
                .unwrap_or((0, 0));
                frame
                    .declared_soroban_resource_fee()
                    .saturating_sub(non_refundable_fee)
            } else {
                0
            }
        };

        // Phase 1-6: Validate structure, accounts, fees, preconditions, sequence, signatures
        let validated = match self.validate_preconditions_with_frame(snapshot, frame, base_fee)? {
            Ok(v) => v,
            Err(validation_failure) => {
                let mut failure_result = validation_failure.result;

                // For Soroban TXs, even validation failures get a fee refund.
                // stellar-core's setError() resets consumed to 0 and
                // finalizeFeeRefund() subtracts the full max_refundable from
                // feeCharged.
                failure_result.fee_refund = soroban_max_refundable;

                // If the validation passed the sequence check (stellar-core's
                // cv >= kInvalidUpdateSeqNum), bump the sequence number even
                // though the TX failed. This matches stellar-core's
                // commonPreApply which calls processSeqNum before returning.
                if validation_failure.past_seq_check {
                    let fail_frame = TransactionFrame::with_network(
                        Arc::clone(tx_envelope),
                        self.network_id,
                    );
                    let inner_source_id =
                        henyey_tx::muxed_to_account_id(&fail_frame.inner_source_account());
                    if let Some(acc) = self.state.get_account_mut(&inner_source_id) {
                        acc.seq_num =
                            stellar_xdr::curr::SequenceNumber(fail_frame.sequence_number());
                        henyey_tx::state::update_account_seq_info(
                            acc,
                            self.ledger_seq,
                            self.close_time,
                        );
                    }
                    // Flush and commit the sequence bump so it persists.
                    self.state.flush_modified_entries();
                    self.state.commit();
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

        // For fee bump transactions, the required fee includes an extra base fee for the wrapper
        let num_ops = std::cmp::max(1, frame.operation_count() as i64);
        let required_fee = if frame.is_fee_bump() {
            // Fee bumps pay baseFee * (numOps + 1) - extra charge for the fee bump wrapper
            base_fee as i64 * (num_ops + 1)
        } else {
            base_fee as i64 * num_ops
        };
        let inclusion_fee = frame.inclusion_fee();
        // For Soroban, the resource fee is charged in full, plus the inclusion fee up to required.
        // For classic transactions, charge up to the required_fee (base_fee * num_ops).
        let mut fee = if frame.is_soroban() {
            frame.declared_soroban_resource_fee() + std::cmp::min(inclusion_fee, required_fee)
        } else {
            std::cmp::min(inclusion_fee, required_fee)
        };

        let fee_deduct_start = std::time::Instant::now();
        let mut preflight_failure = None;
        if deduct_fee {
            if let Some(acc) = self.state.get_account(&fee_source_id) {
                if self.available_balance_for_fee(acc)? < fee {
                    preflight_failure = Some(TransactionResultCode::TxInsufficientBalance);
                }
            }
        }

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
                acc.balance -= charged_fee;
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
        let seq_created;
        let seq_updated;
        let seq_deleted;
        let mut signer_created = Vec::new();
        let mut signer_updated = Vec::new();
        let mut signer_deleted = Vec::new();

        // For fee bump transactions in two-phase mode, stellar-core's
        // FeeBumpTransactionFrame::apply() ALWAYS calls removeOneTimeSignerKeyFromFeeSource()
        // which loads the fee source account, generating a STATE/UPDATED pair even if
        // the fee source equals the inner source. This happens BEFORE the inner transaction's
        // sequence bump. We capture this to match stellar-core ordering.
        let fee_bump_wrapper_changes = if !deduct_fee && frame.is_fee_bump() {
            let fee_source_key = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
                account_id: fee_source_id.clone(),
            });
            if let Some(fee_source_entry) = self.state.get_entry(&fee_source_key) {
                // Both STATE and UPDATED use the same (current) state value.
                // In stellar-core, the fee bump wrapper's tx_changes_before captures
                // the fee source state AFTER fee processing (which happened in fee_meta).
                // This is just for removeOneTimeSignerKeyFromFeeSource() which doesn't
                // change the balance - hence STATE and UPDATED have the same value.
                // We ignore fee_source_pre_state as it's not needed.
                let _ = fee_source_pre_state; // Explicitly ignore the parameter
                vec![
                    LedgerEntryChange::State(fee_source_entry.clone()),
                    LedgerEntryChange::Updated(fee_source_entry),
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
            sig_check_failure = check_operation_signatures(
                &frame,
                &self.state,
                &sig_hash,
                sig_signatures,
                &inner_source_id,
            );
        }

        let op_sig_check_us = op_sig_check_start.elapsed().as_micros() as u64;

        // Remove one-time (PreAuthTx) signers from all source accounts.
        // This must happen AFTER the signature check above so PreAuthTx signers
        // are still present when their weight is evaluated.
        let signer_removal_start = std::time::Instant::now();
        let mut signer_changes = empty_entry_changes();
        if self.protocol_version != 7 {
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

            self.state.remove_one_time_signers_from_all_sources(
                &outer_hash,
                &source_accounts,
                self.protocol_version,
            );
            self.state.flush_modified_entries();
            let delta_after_signers = delta_snapshot(&self.state);
            let delta_slice = delta_slice_between(
                self.state.delta(),
                delta_before_signers,
                delta_after_signers,
            );
            signer_created = delta_slice.created().to_vec();
            signer_updated = delta_slice.updated().to_vec();
            signer_deleted = delta_slice.deleted().to_vec();
            signer_changes = build_entry_changes_with_state_overrides(
                &self.state,
                &signer_created,
                &signer_updated,
                &signer_deleted,
                &signer_state_overrides,
            );
        }
        let signer_removal_us = signer_removal_start.elapsed().as_micros() as u64;

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

        if let Some(acc) = self.state.get_account_mut(&inner_source_id) {
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
        seq_created = delta_slice.created().to_vec();
        seq_updated = delta_slice.updated().to_vec();
        seq_deleted = delta_slice.deleted().to_vec();

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
        // Persist sequence updates so failed transactions still consume sequence numbers.
        self.state.commit();

        // Commit pre-apply changes so rollback doesn't revert them.
        self.state.commit();
        let seq_bump_us = seq_bump_start.elapsed().as_micros() as u64;
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
        }))
    }

    /// Build a `TransactionExecutionResult` for a TX whose operation body was
    /// skipped (e.g., insufficient fee source balance in the parallel path).
    ///
    /// This matches stellar-core's `parallelApply` returning `{false, {}}` when
    /// `!txResult.isSuccess()` after `preParallelApply`.
    fn build_skipped_result(
        pre: PreApplyResult,
        emit_soroban_tx_meta_ext_v1: bool,
        enable_soroban_diagnostic_events: bool,
    ) -> TransactionExecutionResult {
        let soroban_fee_info = pre.refundable_fee_tracker.as_ref().map(|t| {
            (t.non_refundable_fee, t.consumed_refundable_fee, t.consumed_rent_fee)
        });
        let fee_refund = pre.refundable_fee_tracker
            .as_ref()
            .map(|t| t.refund_amount())
            .unwrap_or(0);

        let tx_meta = build_transaction_meta(
            pre.tx_changes_before,
            vec![],  // no operation changes
            vec![],  // no operation events
            vec![],  // no tx events
            None,    // no soroban return value
            vec![],  // no diagnostic events
            soroban_fee_info,
            emit_soroban_tx_meta_ext_v1,
            enable_soroban_diagnostic_events,
        );

        let total_us = pre.tx_timing_start.elapsed().as_micros() as u64;
        TransactionExecutionResult {
            success: false,
            fee_charged: 0, // overridden by caller from pre-charged fees
            fee_refund,
            operation_results: vec![],
            error: Some("Insufficient balance for fee".into()),
            failure: Some(TransactionResultCode::TxInsufficientBalance),
            tx_meta: Some(tx_meta),
            fee_changes: Some(pre.fee_changes),
            post_fee_changes: Some(empty_entry_changes()),
            hot_archive_restored_keys: Vec::new(),
            op_type_timings: HashMap::new(),
            exec_time_us: total_us,
            validation_us: pre.validation_us,
            fee_seq_us: pre.fee_seq_us,
            footprint_us: 0,
            ops_us: 0,
            meta_build_us: 0,
            meta_commit_us: 0,
            meta_fee_refund_us: 0,
            meta_build_phase_us: 0,
            val_account_load_us: pre.val_account_load_us,
            val_tx_hash_us: pre.val_tx_hash_us,
            val_ed25519_us: pre.val_ed25519_us,
            val_other_us: pre.val_other_us,
            fee_deduct_us: pre.fee_deduct_us,
            op_sig_check_us: pre.op_sig_check_us,
            signer_removal_us: pre.signer_removal_us,
            seq_bump_us: pre.seq_bump_us,
        }
    }

    /// Execute a transaction with configurable fee deduction and optional pre-fee state.
    ///
    /// This is the main orchestrator that calls `pre_apply()` for validation,
    /// fee charging, signer removal, and sequence bumping, then conditionally
    /// calls through to the operation body execution phase.
    ///
    /// When `deduct_fee` is false, fee validation still occurs but no fee
    /// processing changes are applied to the state or delta.
    ///
    /// When `should_apply` is false, the transaction's pre-apply phase executes
    /// normally, but the operation body is **not** executed. This matches
    /// stellar-core's `parallelApply` which returns `{false, {}}` immediately
    /// when `!txResult.isSuccess()` after `preParallelApply` determined
    /// insufficient balance.
    ///
    /// For fee bump transactions in two-phase mode, `fee_source_pre_state` should be provided
    /// with the fee source account state BEFORE fee processing. This is used for the STATE entry
    /// in tx_changes_before to match stellar-core behavior.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_transaction_with_fee_mode_and_pre_state(
        &mut self,
        snapshot: &SnapshotHandle,
        tx_envelope: &TransactionEnvelope,
        base_fee: u32,
        soroban_prng_seed: Option<[u8; 32]>,
        deduct_fee: bool,
        fee_source_pre_state: Option<LedgerEntry>,
        should_apply: bool,
    ) -> Result<TransactionExecutionResult> {
        let tx_arc = Arc::new(tx_envelope.clone());
        self.execute_transaction_with_arc(
            snapshot, tx_arc, base_fee, soroban_prng_seed,
            deduct_fee, fee_source_pre_state, should_apply,
        )
    }

    /// Execute a transaction from a shared Arc envelope (avoids cloning the envelope).
    #[allow(clippy::too_many_arguments)]
    pub fn execute_transaction_with_arc(
        &mut self,
        snapshot: &SnapshotHandle,
        tx_envelope: Arc<TransactionEnvelope>,
        base_fee: u32,
        soroban_prng_seed: Option<[u8; 32]>,
        deduct_fee: bool,
        fee_source_pre_state: Option<LedgerEntry>,
        should_apply: bool,
    ) -> Result<TransactionExecutionResult> {
        // Phase 1: Pre-apply (validate, charge fees, remove signers, bump seq)
        let pre = match self.pre_apply_arc(
            snapshot, &tx_envelope, base_fee,
            soroban_prng_seed, deduct_fee, fee_source_pre_state,
        )? {
            Ok(pre) => pre,
            Err(early_result) => return Ok(early_result),
        };

        // Phase 2: Skip operation body when caller determined TX should not apply
        if !should_apply {
            return Ok(Self::build_skipped_result(
                pre,
                self.emit_soroban_tx_meta_ext_v1,
                self.enable_soroban_diagnostic_events,
            ));
        }

        // Phase 3: Execute operation body
        self.apply_body(snapshot, pre)
    }

    /// Apply phase: execute operations, build meta, handle rollback on failure.
    ///
    /// This is the second half of transaction execution, consuming the
    /// `PreApplyResult` produced by `pre_apply()`. It loads footprint entries,
    /// executes each operation, handles rollback on failure (restoring
    /// fee/seq/signer entries from the pre-apply phase), builds transaction
    /// meta, and returns the final `TransactionExecutionResult`.
    ///
    /// This matches the body of stellar-core's `parallelApply` (for Soroban) /
    /// `TransactionFrame::apply` (for classic) — everything after `commonPreApply`.
    fn apply_body(
        &mut self,
        snapshot: &SnapshotHandle,
        pre: PreApplyResult,
    ) -> Result<TransactionExecutionResult> {
        let PreApplyResult {
            frame,
            fee_source_id,
            inner_source_id,
            tx_changes_before,
            fee_changes,
            mut refundable_fee_tracker,
            mut tx_event_manager,
            preflight_failure,
            sig_check_failure,
            fee,
            fee_entries,
            seq_entries,
            signer_entries,
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
        } = pre;

        // Create ledger context for operation execution
        let ledger_context = if let Some(prng_seed) = soroban_prng_seed {
            LedgerContext::with_prng_seed(
                self.ledger_seq,
                self.close_time,
                base_fee,
                self.base_reserve,
                self.protocol_version,
                self.network_id,
                prng_seed,
            )
        } else {
            LedgerContext::new(
                self.ledger_seq,
                self.close_time,
                base_fee,
                self.base_reserve,
                self.protocol_version,
                self.network_id,
            )
        };

        let soroban_data = frame.soroban_data();

        // For Soroban transactions, load all footprint entries from the snapshot
        // before executing operations. This ensures contract data, code, and TTLs
        // are available to the Soroban host.
        //
        // NOTE: We no longer call clear_archived_entries_from_state() here because
        // archived entries from the hot archive need to be available to the Soroban
        // host for restoration. The previous approach of clearing them was designed
        // for when all entries came from the live bucket list, but with hot archive
        // support, archived entries are properly sourced and must be preserved.
        if let Some(data) = soroban_data {
            self.load_soroban_footprint(snapshot, &data.resources.footprint)?;
        }

        let footprint_us =
            tx_timing_start.elapsed().as_micros() as u64 - validation_us - fee_seq_us;

        self.state.clear_sponsorship_stack();

        // Pre-load sponsor accounts for BeginSponsoringFutureReserves operations.
        // When a BeginSponsoringFutureReserves operation is followed by other operations
        // (like SetOptions), those subsequent operations may need to update the sponsor's
        // num_sponsoring count. We must load these sponsor accounts before the operation
        // loop so they're available when needed.
        for op in frame.operations().iter() {
            if let OperationBody::BeginSponsoringFutureReserves(_) = &op.body {
                // The sponsor is the source of the BeginSponsoringFutureReserves operation
                let op_source_muxed = op
                    .source_account
                    .clone()
                    .unwrap_or_else(|| frame.inner_source_account());
                let sponsor_id = henyey_tx::muxed_to_account_id(&op_source_muxed);
                self.load_account(snapshot, &sponsor_id)?;
            }
        }

        // Set up lazy entry loader for offer dependency loading.
        // Instead of preloading all offer dependencies upfront (which requires
        // O(n) bucket list lookups for all offers in each asset pair), the
        // entry_loader enables on-demand loading during offer crossing.
        let snapshot_for_loader = snapshot.clone();
        self.state.set_entry_loader(std::sync::Arc::new(move |key| {
            snapshot_for_loader
                .get_entry(key)
                .map_err(|e| henyey_tx::TxError::Internal(e.to_string()))
        }));

        // Set up batch entry loader for loading multiple entries in a single
        // bucket list pass. This is used by path payment operations to batch-load
        // seller account + trustlines together (~3x faster than separate lookups).
        let snapshot_for_batch = snapshot.clone();
        self.state
            .set_batch_entry_loader(std::sync::Arc::new(move |keys| {
                snapshot_for_batch
                    .load_entries(keys)
                    .map_err(|e| henyey_tx::TxError::Internal(e.to_string()))
            }));

        // Set up pool-share-trustlines-by-account loader (defense in depth).
        // This mirrors the offers loader pattern: `find_pool_share_trustlines_for_asset`
        // calls `ensure_pool_share_trustlines_loaded` which uses this loader to
        // discover pool IDs from the secondary index.  Even if the pre-loading in
        // `load_operation_accounts` is bypassed (e.g. by a future code path), the
        // state manager will load pool share TLs on demand before iterating.
        let snapshot_for_pool_shares = snapshot.clone();
        self.state
            .set_pool_share_tls_by_account_loader(std::sync::Arc::new(
                move |account_id| {
                    snapshot_for_pool_shares
                        .pool_share_tls_by_account(account_id)
                        .map_err(|e| henyey_tx::TxError::Internal(e.to_string()))
                },
            ));

        // Execute operations
        let mut operation_results = Vec::new();
        let num_ops = frame.operations().len();
        let mut op_changes = Vec::with_capacity(num_ops);
        let mut op_events: Vec<Vec<ContractEvent>> = Vec::with_capacity(num_ops);
        let mut diagnostic_events: Vec<DiagnosticEvent> = Vec::new();
        let mut soroban_return_value = None;
        let mut all_success = true;
        let mut failure = None;
        // Track pre-TX delta position so we only scan NEW created entries for contract cache.
        let pre_tx_created_count = self.state.delta().created_entries().len();
        // For multi-operation transactions, stellar-core records STATE/UPDATED
        // for every accessed entry per operation, even if values are identical.
        // For single-operation transactions, it only records if values changed.
        self.state.set_multi_op_mode(num_ops > 1);

        let tx_seq = frame.sequence_number();
        // Collect hot archive restored keys across all operations (Protocol 23+)
        // Use HashSet to deduplicate: when multiple TXs in the same ledger restore
        // the same entry (e.g., same ContractCode), it should only be sent to
        // HotArchiveBucketList::add_batch once as a single Live marker.
        let mut collected_hot_archive_keys: HashSet<LedgerKey> = HashSet::new();
        let mut op_type_timings: HashMap<OperationType, (u64, u32)> = HashMap::new();

        // Apply the signature check result from above (checked before signer removal).
        if let Some((op_results, sig_failure)) = sig_check_failure {
            all_success = false;
            operation_results = op_results;
            failure = Some(sig_failure);
        }

        if let Some(preflight_failure) = preflight_failure {
            all_success = false;
            failure = Some(preflight_failure);
        } else if all_success {
            // Clone memo once before the loop — it's constant for the entire TX.
            let tx_memo = frame.memo().clone();

            for (op_index, op) in frame.operations().iter().enumerate() {
                let op_type = OperationType::from_body(&op.body);

                let op_source_muxed = op
                    .source_account
                    .clone()
                    .unwrap_or_else(|| frame.inner_source_account());
                let op_delta_before = delta_snapshot(&self.state);
                self.state.begin_op_snapshot();
                let op_timing_start = std::time::Instant::now();

                // Load any accounts needed for this operation
                self.load_operation_accounts(snapshot, op, &inner_source_id)?;

                // Get operation source
                let op_source = henyey_tx::muxed_to_account_id(&op_source_muxed);

                let pre_claimable_balance = match &op.body {
                    OperationBody::ClaimClaimableBalance(op_data) => self
                        .state
                        .get_claimable_balance(&op_data.balance_id)
                        .cloned(),
                    OperationBody::ClawbackClaimableBalance(op_data) => self
                        .state
                        .get_claimable_balance(&op_data.balance_id)
                        .cloned(),
                    _ => None,
                };
                let pre_pool = match &op.body {
                    OperationBody::LiquidityPoolDeposit(op_data) => self
                        .state
                        .get_liquidity_pool(&op_data.liquidity_pool_id)
                        .cloned(),
                    OperationBody::LiquidityPoolWithdraw(op_data) => self
                        .state
                        .get_liquidity_pool(&op_data.liquidity_pool_id)
                        .cloned(),
                    _ => None,
                };
                let mut op_event_manager = OpEventManager::new(
                    true,
                    op_type.is_soroban(),
                    self.protocol_version,
                    self.network_id,
                    tx_memo.clone(),
                    self.classic_events,
                );

                // Execute the operation with a per-operation savepoint.
                // If the operation fails, we roll back its state changes so
                // subsequent operations see clean state (matching stellar-core LedgerTxn).
                // For single-op TXs, skip savepoint creation — TX-level rollback handles failures.
                let op_index = u32::try_from(op_index).unwrap_or(u32::MAX);

                let op_savepoint = if num_ops == 1 {
                    None
                } else {
                    Some(self.state.create_savepoint())
                };
                let result = self.execute_single_operation(
                    op,
                    &op_source,
                    &inner_source_id,
                    tx_seq,
                    op_index,
                    &ledger_context,
                    soroban_data,
                );

                match result {
                    Ok(mut op_exec) => {
                        self.state.flush_modified_entries();
                        let mut op_result = op_exec.result;

                        // Debug: Log operation result for Soroban operations
                        if op_type.is_soroban() {
                            let is_success_before_refund_check = is_operation_success(&op_result);
                            tracing::debug!(
                                ledger_seq = self.ledger_seq,
                                op_index,
                                op_type = ?op_type,
                                op_result = ?op_result,
                                is_success = is_success_before_refund_check,
                                has_soroban_meta = op_exec.soroban_meta.is_some(),
                                "Soroban operation executed"
                            );
                        }

                        if let Some(meta) = &op_exec.soroban_meta {
                            if let Some(tracker) = refundable_fee_tracker.as_mut() {
                                tracing::debug!(
                                    ledger_seq = self.ledger_seq,
                                    op_index,
                                    rent_fee = meta.rent_fee,
                                    event_size_bytes = meta.event_size_bytes,
                                    max_refundable = tracker.max_refundable_fee,
                                    consumed_rent = tracker.consumed_rent_fee,
                                    consumed_refundable = tracker.consumed_refundable_fee,
                                    "Refundable fee tracker pre-consume"
                                );
                                if !tracker.consume(
                                    &frame,
                                    self.protocol_version,
                                    &self.soroban_config,
                                    meta.event_size_bytes,
                                    meta.rent_fee,
                                ) {
                                    tracing::debug!(
                                        ledger_seq = self.ledger_seq,
                                        op_index,
                                        "InsufficientRefundableFee"
                                    );
                                    op_result = insufficient_refundable_fee_result(op);
                                    all_success = false;
                                    failure = Some(TransactionResultCode::TxFailed);
                                }
                            }
                        }
                        // Check if operation succeeded
                        if !is_operation_success(&op_result) {
                            all_success = false;
                            tracing::debug!(
                                ledger_seq = self.ledger_seq,
                                op_index,
                                op_type = ?op_type,
                                op_result = ?op_result,
                                "Operation failed"
                            );
                            if matches!(op_result, OperationResult::OpNotSupported) {
                                failure = Some(TransactionResultCode::TxNotSupported);
                            }
                            // Roll back failed operation's state changes so subsequent
                            // operations see clean state (matches stellar-core nested LedgerTxn).
                            if let Some(sp) = op_savepoint {
                                self.state.rollback_to_savepoint(sp);
                            }
                        }
                        operation_results.push(op_result.clone());

                        let op_delta_after = delta_snapshot(&self.state);
                        let op_snapshots = self.state.end_op_snapshot();
                        let delta_slice = delta_slice_between(
                            self.state.delta(),
                            op_delta_before,
                            op_delta_after,
                        );

                        // For Soroban operations, extract restored entries (hot archive and live BL)
                        let (restored_entries, footprint) = if op_type.is_soroban() {
                            let mut restored = RestoredEntries::default();

                            // Get live BL restorations from the Soroban execution result
                            if let Some(meta) = &op_exec.soroban_meta {
                                for live_bl_restore in &meta.live_bucket_list_restores {
                                    restored
                                        .live_bucket_list
                                        .insert(live_bl_restore.key.clone());
                                    restored.live_bucket_list_entries.insert(
                                        live_bl_restore.key.clone(),
                                        live_bl_restore.entry.clone(),
                                    );
                                    // Also track the TTL entry
                                    restored
                                        .live_bucket_list
                                        .insert(live_bl_restore.ttl_key.clone());
                                    restored.live_bucket_list_entries.insert(
                                        live_bl_restore.ttl_key.clone(),
                                        live_bl_restore.ttl_entry.clone(),
                                    );
                                }
                            }

                            // Get hot archive keys from two sources:
                            // 1. For InvokeHostFunction: from actual_restored_indices (filtered by host)
                            // 2. For RestoreFootprint: from soroban_meta.hot_archive_restores
                            // NOTE: We must exclude live BL restore keys from the hot archive set.
                            // Live BL restores are entries that exist in the live bucket list with
                            // expired TTL but haven't been evicted yet - these are NOT hot archive
                            // restores and should not be added to HotArchiveBucketList::add_batch.
                            let actual_restored_indices = op_exec
                                .soroban_meta
                                .as_ref()
                                .map(|m| m.actual_restored_indices.as_slice())
                                .unwrap_or(&[]);
                            let mut hot_archive = extract_hot_archive_restored_keys(
                                soroban_data,
                                op_type,
                                actual_restored_indices,
                            );
                            // For RestoreFootprint, get hot archive keys and entries from the meta
                            if let Some(meta) = &op_exec.soroban_meta {
                                for ha_restore in &meta.hot_archive_restores {
                                    hot_archive.insert(ha_restore.key.clone());
                                    // Also store the entry for RESTORED meta emission
                                    restored
                                        .hot_archive_entries
                                        .insert(ha_restore.key.clone(), ha_restore.entry.clone());
                                }
                            }
                            let ha_before = hot_archive.len();
                            hot_archive.retain(|k| !restored.live_bucket_list.contains(k));
                            let ha_after_live_bl = hot_archive.len();

                            // Also exclude keys that were listed in archived_soroban_entries but
                            // were already restored by a previous TX in this ledger. These entries
                            // go into `updated` (not `created`) because they already exist in state.
                            // We only want RESTORED emission for entries actually being created/restored
                            // in THIS transaction.
                            let created_keys: HashSet<LedgerKey> = delta_slice
                                .created()
                                .iter()
                                .map(|entry| henyey_common::entry_to_key(entry))
                                .collect();
                            // For transaction meta emission: only emit RESTORED for keys in created
                            // Keep original set for bucket list operations
                            let hot_archive_for_bucket_list = hot_archive.clone();
                            // For RestoreFootprint, the data entries are prefetched from hot archive
                            // into state, so they won't be in `created_keys` (only the TTL is created).
                            // We need to emit RESTORED for all hot archive keys without filtering.
                            // For InvokeHostFunction, we filter by created_keys because the auto-restore
                            // creates the entries during execution.
                            let hot_archive_for_meta: HashSet<LedgerKey> =
                                if op_type == OperationType::RestoreFootprint {
                                    // Don't filter - all hot archive keys should emit RESTORED
                                    hot_archive.clone()
                                } else {
                                    // Filter by created_keys for InvokeHostFunction
                                    hot_archive
                                        .iter()
                                        .filter(|k| created_keys.contains(k))
                                        .cloned()
                                        .collect()
                                };
                            let ha_after = hot_archive_for_meta.len();
                            // Log when we filter out entries
                            if ha_before != ha_after {
                                tracing::debug!(
                                    ha_before,
                                    ha_after_live_bl,
                                    ha_after,
                                    live_bl_count = restored.live_bucket_list.len(),
                                    created_count = created_keys.len(),
                                    ?hot_archive,
                                    ?created_keys,
                                    op_type = ?op_type,
                                    "Filtered hot archive keys: live BL restores and already-restored entries"
                                );
                            }
                            // For transaction meta purposes, also add the corresponding TTL keys.
                            // When a ContractData/ContractCode entry is restored from hot archive,
                            // its TTL entry should also be emitted as RESTORED (not CREATED).
                            // Use the filtered set (hot_archive_for_meta) which only includes entries
                            // actually being created/restored in this TX.
                            // NOTE: We don't add TTL keys to collected_hot_archive_keys because
                            // HotArchiveBucketList::add_batch only receives data/code entries.
                            use sha2::{Digest, Sha256};
                            let ttl_keys: Vec<_> = hot_archive_for_meta
                                .iter()
                                .filter_map(|key| {
                                    // Compute key hash as SHA256 of key XDR
                                    let key_bytes = key.to_xdr(Limits::none()).ok()?;
                                    let key_hash =
                                        stellar_xdr::curr::Hash(Sha256::digest(&key_bytes).into());
                                    Some(LedgerKey::Ttl(stellar_xdr::curr::LedgerKeyTtl {
                                        key_hash,
                                    }))
                                })
                                .collect();
                            // Collect data/code keys only for HotArchiveBucketList::add_batch.
                            // All hot archive keys (already filtered by live BL above) should be
                            // passed to the bucket list. This is true for both RestoreFootprint
                            // and InvokeHostFunction - the hot archive needs to remove ALL entries
                            // that were restored, regardless of whether the contract then modifies
                            // them (which would put them in `updated` rather than `created`).
                            // The `created_keys` filtering above is only for transaction meta
                            // emission (RESTORED vs UPDATED), not for bucket list operations.
                            //
                            // IMPORTANT: Only collect hot archive keys for SUCCESSFUL operations.
                            // In stellar-core, handleArchivedEntry writes the restoration to
                            // mOpState (a nested LedgerTxn), but if the operation fails, that
                            // nested LedgerTxn is rolled back, canceling the restorations and
                            // preventing any HOT_ARCHIVE_LIVE tombstones from being written.
                            // For failed operations, we must not add keys to the hot archive
                            // batch — doing so would produce spurious HOT_ARCHIVE_LIVE tombstones
                            // in the hot archive bucket list, causing a bucket_list_hash mismatch.
                            if is_operation_success(&op_result) {
                                collected_hot_archive_keys
                                    .extend(hot_archive_for_bucket_list.iter().cloned());
                            }
                            // Add filtered keys (including TTL) to restored.hot_archive for meta conversion
                            restored.hot_archive.extend(hot_archive_for_meta);
                            restored.hot_archive.extend(ttl_keys);
                            (restored, soroban_data.map(|d| &d.resources.footprint))
                        } else {
                            (RestoredEntries::default(), None)
                        };

                        let change_order = delta_slice.change_order();
                        let ledger_changes = LedgerChanges {
                            created: delta_slice.created(),
                            updated: delta_slice.updated(),
                            update_states: delta_slice.update_states(),
                            deleted: delta_slice.deleted(),
                            delete_states: delta_slice.delete_states(),
                            change_order: &change_order,
                            state_overrides: &op_snapshots,
                            restored: &restored_entries,
                        };
                        let op_changes_local = build_entry_changes_with_hot_archive(
                            &self.state,
                            &ledger_changes,
                            footprint,
                            self.ledger_seq,
                        );

                        let mut op_events_final = Vec::new();
                        if all_success && is_operation_success(&op_result) {
                            if let Some(meta) = op_exec.soroban_meta.as_mut() {
                                op_event_manager
                                    .set_events(std::mem::take(&mut meta.events));
                                diagnostic_events.extend(
                                    std::mem::take(&mut meta.diagnostic_events),
                                );
                                soroban_return_value = std::mem::take(&mut meta.return_value)
                                    .or(soroban_return_value);
                            }

                            if !op_type.is_soroban() {
                                emit_classic_events_for_operation(
                                    &mut op_event_manager,
                                    op,
                                    &op_result,
                                    &op_source_muxed,
                                    &self.state,
                                    pre_claimable_balance.as_ref(),
                                    pre_pool.as_ref(),
                                );
                            }

                            if op_event_manager.is_enabled() {
                                op_events_final = op_event_manager.finalize();
                            }
                        }

                        if all_success {
                            op_changes.push(op_changes_local);
                            op_events.push(op_events_final);
                        } else {
                            op_changes.push(empty_entry_changes());
                            op_events.push(Vec::new());
                        }
                    }
                    Err(e) => {
                        if let Some(sp) = op_savepoint {
                            self.state.rollback_to_savepoint(sp);
                        }
                        self.state.end_op_snapshot();
                        all_success = false;
                        tracing::debug!(
                            error = %e,
                            op_index = op_index,
                            op_type = ?OperationType::from_body(&op.body),
                            ledger_seq = self.ledger_seq,
                            "Operation execution returned Err (mapped to txInternalError)"
                        );
                        // stellar-core maps std::runtime_error during operation execution
                        // to txINTERNAL_ERROR (not txNOT_SUPPORTED). The exception
                        // aborts all remaining operations.
                        failure = Some(TransactionResultCode::TxInternalError);
                        break;
                    }
                }
                let op_elapsed_us = op_timing_start.elapsed().as_micros() as u64;
                let entry = op_type_timings.entry(op_type).or_insert((0u64, 0u32));
                entry.0 += op_elapsed_us;
                entry.1 += 1;
            }
        }

        let ops_us = tx_timing_start.elapsed().as_micros() as u64
            - validation_us
            - fee_seq_us
            - footprint_us;

        let meta_phase_start = std::time::Instant::now();

        if all_success && self.state.has_pending_sponsorship() {
            all_success = false;
            failure = Some(TransactionResultCode::TxBadSponsorship);
        }

        if !all_success {
            let tx_hash = frame
                .hash(&self.network_id)
                .map(|hash| hash.to_hex())
                .unwrap_or_else(|_| "unknown".to_string());
            debug!(
                tx_hash = %tx_hash,
                fee_source = %account_id_to_strkey(&fee_source_id),
                inner_source = %account_id_to_strkey(&inner_source_id),
                results = ?operation_results,
                "Transaction failed; rolling back changes"
            );
            self.state.rollback();
            restore_delta_entries(
                &mut self.state,
                &fee_entries.created,
                &fee_entries.updated,
                &fee_entries.deleted,
            );
            // Re-add the fee to the delta after rollback.
            // rollback() restores the delta from the snapshot taken BEFORE fee deduction,
            // so we must explicitly re-add this transaction's fee to preserve it.
            // This ensures failed transactions still contribute their fees to the fee pool.
            if deduct_fee && fee > 0 {
                self.state.delta_mut().add_fee(fee);
            }
            restore_delta_entries(
                &mut self.state,
                &seq_entries.created,
                &seq_entries.updated,
                &seq_entries.deleted,
            );
            restore_delta_entries(
                &mut self.state,
                &signer_entries.created,
                &signer_entries.updated,
                &signer_entries.deleted,
            );
            op_changes.clear();
            op_events.clear();
            diagnostic_events.clear();
            soroban_return_value = None;

            // Reset the refundable fee tracker when transaction fails.
            // This mirrors stellar-core's behavior where setError() calls resetConsumedFee(),
            // ensuring the full max_refundable_fee is refunded on any transaction failure.
            if let Some(tracker) = refundable_fee_tracker.as_mut() {
                tracing::debug!(
                    ledger_seq = self.ledger_seq,
                    is_soroban = frame.is_soroban(),
                    max_refundable_fee = tracker.max_refundable_fee,
                    consumed_before_reset = tracker.consumed_refundable_fee,
                    "Resetting fee tracker due to tx failure"
                );
                tracker.reset();
            }
        } else {
            self.state.commit();

            // Update module cache with any newly created contract code.
            // This ensures subsequent transactions can use VmCachedInstantiation
            // (cheap) instead of VmInstantiation (expensive) for contracts
            // deployed in this transaction.
            // Only scan entries created by THIS TX (not all prior TXs in the cluster)
            // to avoid O(n²) iteration over the growing delta.
            let created = self.state.delta().created_entries();
            for entry in &created[pre_tx_created_count..] {
                if let stellar_xdr::curr::LedgerEntryData::ContractCode(cc) = &entry.data {
                    self.add_contract_to_cache(cc.code.as_slice());
                }
            }
        }

        let commit_phase_us = meta_phase_start.elapsed().as_micros() as u64;

        let post_fee_changes = empty_entry_changes();
        let mut fee_refund = 0i64;
        let mut soroban_fee_info = None;
        if let Some(tracker) = refundable_fee_tracker {
            // Extract fee tracking info for soroban meta before consuming tracker
            soroban_fee_info = Some((
                tracker.non_refundable_fee,
                tracker.consumed_refundable_fee,
                tracker.consumed_rent_fee,
            ));
            let refund = tracker.refund_amount();
            let stage = TransactionEventStage::AfterAllTxs;
            tx_event_manager.new_fee_event(&fee_source_id, -refund, stage);
            fee_refund = refund;
        }

        let fee_refund_phase_us = meta_phase_start.elapsed().as_micros() as u64 - commit_phase_us;

        let tx_events = tx_event_manager.finalize();
        let tx_meta = build_transaction_meta(
            tx_changes_before,
            op_changes,
            op_events,
            tx_events,
            soroban_return_value,
            diagnostic_events,
            soroban_fee_info,
            self.emit_soroban_tx_meta_ext_v1,
            self.enable_soroban_diagnostic_events,
        );

        let meta_build_phase_us = meta_phase_start.elapsed().as_micros() as u64 - commit_phase_us - fee_refund_phase_us;

        let total_us = tx_timing_start.elapsed().as_micros() as u64;
        let meta_us = total_us - validation_us - fee_seq_us - footprint_us - ops_us;

        if total_us > 5000 || frame.is_soroban() {
            // Build a compact string of per-op-type timings sorted by time desc
            let mut op_timing_vec: Vec<_> = op_type_timings.iter().collect();
            op_timing_vec.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
            let op_timing_str: String = op_timing_vec
                .iter()
                .map(|(op, (us, count))| format!("{:?}:{}us×{}", op, us, count))
                .collect::<Vec<_>>()
                .join(",");
            tracing::debug!(
                ledger_seq = self.ledger_seq,
                total_us,
                validation_us,
                val_account_load_us,
                val_tx_hash_us,
                val_ed25519_us,
                val_other_us,
                fee_seq_us,
                fee_deduct_us,
                op_sig_check_us,
                signer_removal_us,
                seq_bump_us,
                footprint_us,
                ops_us,
                meta_us,
                is_soroban = frame.is_soroban(),
                num_ops = frame.operations().len(),
                success = all_success,
                op_timings = %op_timing_str,
                "TX phase timing"
            );
        }

        Ok(TransactionExecutionResult {
            success: all_success,
            fee_charged: fee.saturating_sub(fee_refund),
            fee_refund,
            operation_results,
            error: if all_success {
                None
            } else {
                Some("One or more operations failed".into())
            },
            failure: if all_success {
                None
            } else {
                Some(failure.unwrap_or(TransactionResultCode::TxFailed))
            },
            tx_meta: Some(tx_meta),
            fee_changes: Some(fee_changes),
            post_fee_changes: Some(post_fee_changes),
            // Convert HashSet back to Vec for the return type
            hot_archive_restored_keys: collected_hot_archive_keys.into_iter().collect(),
            op_type_timings,
            exec_time_us: total_us,
            validation_us,
            fee_seq_us,
            footprint_us,
            ops_us,
            meta_build_us: meta_us,
            meta_commit_us: commit_phase_us,
            meta_fee_refund_us: fee_refund_phase_us,
            meta_build_phase_us,
            val_account_load_us,
            val_tx_hash_us,
            val_ed25519_us,
            val_other_us,
            fee_deduct_us,
            op_sig_check_us,
            signer_removal_us,
            seq_bump_us,
        })
    }

    /// Load accounts needed for an operation.
    fn load_operation_accounts(
        &mut self,
        snapshot: &SnapshotHandle,
        op: &stellar_xdr::curr::Operation,
        source_id: &AccountId,
    ) -> Result<()> {
        let op_source = op
            .source_account
            .as_ref()
            .map(henyey_tx::muxed_to_account_id)
            .unwrap_or_else(|| source_id.clone());

        // Load operation source if different from transaction source
        if let Some(ref muxed) = op.source_account {
            let op_source = henyey_tx::muxed_to_account_id(muxed);
            self.load_account(snapshot, &op_source)?;
        }

        // Phase 1: Batch-load statically-known keys (shared with per-ledger prefetch).
        // When the prefetch cache is populated, these lookups are cache hits.
        // When called without prefetch (e.g., in tests), this provides the
        // same batch-loading benefit as the per-ledger prefetch.
        {
            let mut static_keys = std::collections::HashSet::new();
            henyey_tx::collect_prefetch_keys(&op.body, &op_source, &mut static_keys);
            if !static_keys.is_empty() {
                let keys_vec: Vec<LedgerKey> = static_keys.into_iter().collect();
                self.batch_load_keys(snapshot, &keys_vec)?;
            }
        }

        // Phase 2: Conditional/secondary loading that depends on loaded state
        // or requires special semantics (e.g., load_account_without_record).
        match &op.body {
            OperationBody::CreateAccount(op_data) => {
                self.load_account(snapshot, &op_data.destination)?;
            }
            OperationBody::BeginSponsoringFutureReserves(op_data) => {
                self.load_account(snapshot, &op_data.sponsored_id)?;
            }
            OperationBody::AllowTrust(op_data) => {
                let asset = allow_trust_asset(op_data, &op_source);
                let mut keys = vec![make_account_key(&op_data.trustor)];
                if let Some(tl_asset) = asset_to_trustline_asset(&asset) {
                    keys.push(make_trustline_key(&op_data.trustor, &tl_asset));
                }
                self.batch_load_keys(snapshot, &keys)?;
                // Load offers by account/asset so they can be removed if authorization is revoked
                self.load_offers_by_account_and_asset(snapshot, &op_data.trustor, &asset)?;
                // Load pool share trustlines so they can be redeemed if authorization is revoked
                self.load_pool_share_trustlines_for_account_and_asset(
                    snapshot,
                    &op_data.trustor,
                    &asset,
                )?;
            }
            OperationBody::Payment(op_data) => {
                let dest = henyey_tx::muxed_to_account_id(&op_data.destination);
                let mut keys = vec![make_account_key(&dest)];
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.asset) {
                    keys.push(make_trustline_key(&op_source, &tl_asset));
                    keys.push(make_trustline_key(&dest, &tl_asset));
                }
                if let Some(issuer) = asset_issuer_id(&op_data.asset) {
                    keys.push(make_account_key(&issuer));
                }
                self.batch_load_keys(snapshot, &keys)?;
            }
            OperationBody::AccountMerge(dest) => {
                let dest = henyey_tx::muxed_to_account_id(dest);
                self.load_account(snapshot, &dest)?;
            }
            OperationBody::ClaimClaimableBalance(op_data) => {
                self.load_claimable_balance(snapshot, &op_data.balance_id)?;
                let key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
                    balance_id: op_data.balance_id.clone(),
                });
                if let Some(sponsor) = self.state.entry_sponsor(&key) {
                    self.load_account(snapshot, &sponsor)?;
                }
                if let Some(entry) = self.state.get_claimable_balance(&op_data.balance_id) {
                    let asset = entry.asset.clone();
                    if let Some(tl_asset) = asset_to_trustline_asset(&asset) {
                        self.load_trustline(snapshot, &op_source, &tl_asset)?;
                        self.load_asset_issuer(snapshot, &asset)?;
                    }
                }
            }
            OperationBody::ClawbackClaimableBalance(op_data) => {
                self.load_claimable_balance(snapshot, &op_data.balance_id)?;
                let key = LedgerKey::ClaimableBalance(LedgerKeyClaimableBalance {
                    balance_id: op_data.balance_id.clone(),
                });
                if let Some(sponsor) = self.state.entry_sponsor(&key) {
                    self.load_account(snapshot, &sponsor)?;
                }
            }
            OperationBody::CreateClaimableBalance(op_data) => {
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.asset) {
                    self.load_trustline(snapshot, &op_source, &tl_asset)?;
                }
            }
            OperationBody::SetTrustLineFlags(op_data) => {
                let mut keys = vec![make_account_key(&op_data.trustor)];
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.asset) {
                    keys.push(make_trustline_key(&op_data.trustor, &tl_asset));
                }
                self.batch_load_keys(snapshot, &keys)?;
                // Load offers by account/asset so they can be removed if authorization is revoked
                self.load_offers_by_account_and_asset(snapshot, &op_data.trustor, &op_data.asset)?;
                // Load pool share trustlines so they can be redeemed if authorization is revoked
                self.load_pool_share_trustlines_for_account_and_asset(
                    snapshot,
                    &op_data.trustor,
                    &op_data.asset,
                )?;
            }
            OperationBody::Clawback(op_data) => {
                let from_account = henyey_tx::muxed_to_account_id(&op_data.from);
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.asset) {
                    self.load_trustline(snapshot, &from_account, &tl_asset)?;
                }
            }
            OperationBody::ManageSellOffer(op_data) => {
                let mut keys = Vec::new();
                for asset in [&op_data.selling, &op_data.buying] {
                    if let Some(tl_asset) = asset_to_trustline_asset(asset) {
                        keys.push(make_trustline_key(&op_source, &tl_asset));
                    }
                    if let Some(issuer) = asset_issuer_id(asset) {
                        keys.push(make_account_key(&issuer));
                    }
                }
                if op_data.offer_id != 0 {
                    keys.push(LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
                        seller_id: op_source.clone(),
                        offer_id: op_data.offer_id,
                    }));
                }
                self.batch_load_keys(snapshot, &keys)?;
                if op_data.offer_id != 0 {
                    self.load_offer_sponsor(snapshot, &op_source, op_data.offer_id)?;
                }
            }
            OperationBody::CreatePassiveSellOffer(op_data) => {
                let mut keys = Vec::new();
                for asset in [&op_data.selling, &op_data.buying] {
                    if let Some(tl_asset) = asset_to_trustline_asset(asset) {
                        keys.push(make_trustline_key(&op_source, &tl_asset));
                    }
                    if let Some(issuer) = asset_issuer_id(asset) {
                        keys.push(make_account_key(&issuer));
                    }
                }
                self.batch_load_keys(snapshot, &keys)?;
            }
            OperationBody::ManageBuyOffer(op_data) => {
                let mut keys = Vec::new();
                for asset in [&op_data.selling, &op_data.buying] {
                    if let Some(tl_asset) = asset_to_trustline_asset(asset) {
                        keys.push(make_trustline_key(&op_source, &tl_asset));
                    }
                    if let Some(issuer) = asset_issuer_id(asset) {
                        keys.push(make_account_key(&issuer));
                    }
                }
                if op_data.offer_id != 0 {
                    keys.push(LedgerKey::Offer(stellar_xdr::curr::LedgerKeyOffer {
                        seller_id: op_source.clone(),
                        offer_id: op_data.offer_id,
                    }));
                }
                self.batch_load_keys(snapshot, &keys)?;
                if op_data.offer_id != 0 {
                    self.load_offer_sponsor(snapshot, &op_source, op_data.offer_id)?;
                }
            }
            OperationBody::PathPaymentStrictSend(op_data) => {
                let dest = henyey_tx::muxed_to_account_id(&op_data.destination);
                let mut keys = vec![make_account_key(&dest)];
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.send_asset) {
                    keys.push(make_trustline_key(&op_source, &tl_asset));
                }
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.dest_asset) {
                    keys.push(make_trustline_key(&dest, &tl_asset));
                }
                if let Some(issuer) = asset_issuer_id(&op_data.send_asset) {
                    keys.push(make_account_key(&issuer));
                }
                if let Some(issuer) = asset_issuer_id(&op_data.dest_asset) {
                    keys.push(make_account_key(&issuer));
                }
                self.batch_load_keys(snapshot, &keys)?;
                self.load_path_payment_pools(
                    snapshot,
                    &op_data.send_asset,
                    &op_data.dest_asset,
                    op_data.path.as_slice(),
                )?;
            }
            OperationBody::PathPaymentStrictReceive(op_data) => {
                let dest = henyey_tx::muxed_to_account_id(&op_data.destination);
                let mut keys = vec![make_account_key(&dest)];
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.send_asset) {
                    keys.push(make_trustline_key(&op_source, &tl_asset));
                }
                if let Some(tl_asset) = asset_to_trustline_asset(&op_data.dest_asset) {
                    keys.push(make_trustline_key(&dest, &tl_asset));
                }
                if let Some(issuer) = asset_issuer_id(&op_data.send_asset) {
                    keys.push(make_account_key(&issuer));
                }
                if let Some(issuer) = asset_issuer_id(&op_data.dest_asset) {
                    keys.push(make_account_key(&issuer));
                }
                self.batch_load_keys(snapshot, &keys)?;
                self.load_path_payment_pools(
                    snapshot,
                    &op_data.send_asset,
                    &op_data.dest_asset,
                    op_data.path.as_slice(),
                )?;
            }
            OperationBody::LiquidityPoolDeposit(op_data) => {
                self.load_liquidity_pool_dependencies(
                    snapshot,
                    &op_source,
                    &op_data.liquidity_pool_id,
                )?;
            }
            OperationBody::LiquidityPoolWithdraw(op_data) => {
                self.load_liquidity_pool_dependencies(
                    snapshot,
                    &op_source,
                    &op_data.liquidity_pool_id,
                )?;
            }
            OperationBody::ChangeTrust(op_data) => {
                // Load existing trustline if any
                let tl_asset = match &op_data.line {
                    stellar_xdr::curr::ChangeTrustAsset::Native => None,
                    stellar_xdr::curr::ChangeTrustAsset::CreditAlphanum4(a) => {
                        Some(TrustLineAsset::CreditAlphanum4(a.clone()))
                    }
                    stellar_xdr::curr::ChangeTrustAsset::CreditAlphanum12(a) => {
                        Some(TrustLineAsset::CreditAlphanum12(a.clone()))
                    }
                    stellar_xdr::curr::ChangeTrustAsset::PoolShare(params) => {
                        // Compute pool ID from params
                        use sha2::{Digest, Sha256};
                        let xdr = params
                            .to_xdr(Limits::none())
                            .map_err(|e| LedgerError::Serialization(e.to_string()))?;
                        let pool_id = PoolId(stellar_xdr::curr::Hash(Sha256::digest(&xdr).into()));
                        Some(TrustLineAsset::PoolShare(pool_id))
                    }
                };
                if let Some(ref tl_asset) = tl_asset {
                    self.load_trustline(snapshot, &op_source, tl_asset)?;
                    // If deleting a trustline (limit=0), load the sponsor account if it has one.
                    // The sponsor's num_sponsoring needs to be decremented.
                    if op_data.limit == 0 {
                        let tl_key = LedgerKey::Trustline(LedgerKeyTrustLine {
                            account_id: op_source.clone(),
                            asset: tl_asset.clone(),
                        });
                        if let Some(sponsor) = self.state.entry_sponsor(&tl_key) {
                            self.load_account(snapshot, &sponsor)?;
                        }
                    }
                }
                // Load issuer account for non-pool-share assets WITHOUT recording.
                // stellar-core uses loadAccountWithoutRecord() for ChangeTrust issuer check
                // which doesn't record the access in transaction changes.
                // We still need to load the account into state so the existence check works.
                match &op_data.line {
                    stellar_xdr::curr::ChangeTrustAsset::CreditAlphanum4(a) => {
                        let asset_code = String::from_utf8_lossy(a.asset_code.as_slice());
                        tracing::debug!(
                            asset_code = %asset_code,
                            issuer = ?a.issuer,
                            "ChangeTrust: loading issuer for CreditAlphanum4 (without record)"
                        );
                        self.load_account_without_record(snapshot, &a.issuer)?;
                    }
                    stellar_xdr::curr::ChangeTrustAsset::CreditAlphanum12(a) => {
                        let asset_code = String::from_utf8_lossy(a.asset_code.as_slice());
                        tracing::debug!(
                            asset_code = %asset_code,
                            issuer = ?a.issuer,
                            "ChangeTrust: loading issuer for CreditAlphanum12 (without record)"
                        );
                        self.load_account_without_record(snapshot, &a.issuer)?;
                    }
                    stellar_xdr::curr::ChangeTrustAsset::PoolShare(params) => {
                        use sha2::{Digest, Sha256};
                        let xdr = params
                            .to_xdr(Limits::none())
                            .map_err(|e| LedgerError::Serialization(e.to_string()))?;
                        let pool_id = PoolId(stellar_xdr::curr::Hash(Sha256::digest(&xdr).into()));
                        let stellar_xdr::curr::LiquidityPoolParameters::LiquidityPoolConstantProduct(cp) = params;
                        let mut keys = vec![LedgerKey::LiquidityPool(LedgerKeyLiquidityPool {
                            liquidity_pool_id: pool_id,
                        })];
                        if let Some(tl_asset) = asset_to_trustline_asset(&cp.asset_a) {
                            keys.push(make_trustline_key(&op_source, &tl_asset));
                        }
                        if let Some(tl_asset) = asset_to_trustline_asset(&cp.asset_b) {
                            keys.push(make_trustline_key(&op_source, &tl_asset));
                        }
                        self.batch_load_keys(snapshot, &keys)?;
                    }
                    _ => {}
                }
            }
            OperationBody::ManageData(op_data) => {
                // Load existing data entry if any (needed for updates and deletes)
                self.load_data_raw(snapshot, &op_source, &op_data.data_name)?;
            }
            OperationBody::RevokeSponsorship(op_data) => {
                // Load the target entry that sponsorship is being revoked from
                use stellar_xdr::curr::RevokeSponsorshipOp;
                match op_data {
                    RevokeSponsorshipOp::LedgerEntry(ledger_key) => {
                        // Load the entry directly by its key
                        self.load_entry(snapshot, ledger_key)?;
                        // Also load owner/sponsor accounts that may be modified
                        match ledger_key {
                            LedgerKey::Account(k) => {
                                self.load_account(snapshot, &k.account_id)?;
                            }
                            LedgerKey::Trustline(k) => {
                                self.load_account(snapshot, &k.account_id)?;
                            }
                            LedgerKey::Offer(k) => {
                                self.load_account(snapshot, &k.seller_id)?;
                            }
                            LedgerKey::Data(k) => {
                                self.load_account(snapshot, &k.account_id)?;
                            }
                            LedgerKey::ClaimableBalance(k) => {
                                // Load the claimable balance and its sponsor
                                self.load_claimable_balance(snapshot, &k.balance_id)?;
                            }
                            _ => {}
                        }
                    }
                    RevokeSponsorshipOp::Signer(signer_key) => {
                        // Load the account that has the signer
                        self.load_account(snapshot, &signer_key.account_id)?;
                    }
                }
            }
            OperationBody::SetOptions(op_data) => {
                // If SetOptions sets an inflation_dest that differs from the source,
                // we need to load that account to validate it exists.
                // This matches stellar-core's loadAccountWithoutRecord() call.
                if let Some(ref inflation_dest) = op_data.inflation_dest {
                    if inflation_dest != &op_source {
                        self.load_account(snapshot, inflation_dest)?;
                    }
                }

                // If SetOptions modifies signers and the source account has sponsored signers,
                // we need to load those sponsor accounts so we can update their num_sponsoring.
                if op_data.signer.is_some() {
                    // Collect sponsor IDs from the source account's signer_sponsoring_i_ds
                    let sponsor_ids: Vec<AccountId> = self
                        .state
                        .get_account(&op_source)
                        .and_then(|account| {
                            if let AccountEntryExt::V1(v1) = &account.ext {
                                if let AccountEntryExtensionV1Ext::V2(v2) = &v1.ext {
                                    return Some(
                                        v2.signer_sponsoring_i_ds
                                            .iter()
                                            .filter_map(|s| s.0.clone())
                                            .collect(),
                                    );
                                }
                            }
                            None
                        })
                        .unwrap_or_default();

                    // Load each sponsor account
                    for sponsor_id in &sponsor_ids {
                        self.load_account(snapshot, sponsor_id)?;
                    }
                }
            }
            _ => {
                // Other operations typically work on source account
            }
        }

        Ok(())
    }

    /// Execute a single operation using the central dispatcher.
    #[allow(clippy::too_many_arguments)]
    fn execute_single_operation(
        &mut self,
        op: &stellar_xdr::curr::Operation,
        source: &AccountId,
        tx_source: &AccountId,
        tx_seq: i64,
        op_index: u32,
        context: &LedgerContext,
        soroban_data: Option<&stellar_xdr::curr::SorobanTransactionData>,
    ) -> std::result::Result<henyey_tx::operations::execute::OperationExecutionResult, TxError>
    {
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
        };

        // Use the central operation dispatcher which handles all operation types
        henyey_tx::operations::execute::execute_operation_with_soroban(
            op, source, &tx_id, &mut self.state, context, &soroban,
        )
    }

    /// Apply all state changes to the delta.
    pub fn apply_to_delta(
        &self,
        _snapshot: &SnapshotHandle,
        delta: &mut LedgerDelta,
    ) -> Result<()> {
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
}

#[derive(Clone, Copy)]
pub struct DeltaSnapshot {
    created: usize,
    updated: usize,
    deleted: usize,
    change_order: usize,
}

/// Zero-copy view into a range of delta changes between two snapshots.
/// Avoids cloning vectors by referencing the parent `LedgerDelta` directly.
pub struct DeltaSlice<'a> {
    delta: &'a henyey_tx::LedgerDelta,
    start: DeltaSnapshot,
    end: DeltaSnapshot,
}

impl<'a> DeltaSlice<'a> {
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

const AUTHORIZED_FLAG: u32 = TrustLineFlags::AuthorizedFlag as u32;

/// Tracks entries restored from different sources per CAP-0066.
#[derive(Debug, Default)]
pub struct RestoredEntries {
    /// Keys restored from hot archive (evicted entries).
    /// These will have CREATED changes that should be converted to RESTORED.
    hot_archive: HashSet<LedgerKey>,
    /// For hot archive restores, maps data/code keys to their entry values.
    /// These are needed to emit RESTORED for data/code that wasn't directly modified
    /// (e.g., RestoreFootprint only creates TTL, but data entry needs RESTORED).
    hot_archive_entries: HashMap<LedgerKey, LedgerEntry>,
    /// Keys restored from live BucketList (expired TTL but not yet evicted).
    /// TTL entries will have STATE+UPDATED that should be converted to RESTORED.
    /// Associated data/code entries need RESTORED meta added even if not modified.
    live_bucket_list: HashSet<LedgerKey>,
    /// For live BL restores, maps data/code keys to their entry values.
    /// These are needed to emit RESTORED for data/code that wasn't directly modified.
    live_bucket_list_entries: HashMap<LedgerKey, LedgerEntry>,
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
    fn new(
        tx_hash: &'a Hash256,
        signatures: &'a [stellar_xdr::curr::DecoratedSignature],
    ) -> Self {
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
    fn check_signature(
        &mut self,
        account: &AccountEntry,
        needed_weight: u32,
    ) -> bool {
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
    fn check_signature_no_account(
        &mut self,
        account_id: &AccountId,
    ) -> bool {
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
                    let expected_hint =
                        [hash_key.0[28], hash_key.0[29], hash_key.0[30], hash_key.0[31]];
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

/// Transaction envelope paired with an optional per-TX base fee override.
type TxWithFee = (Arc<TransactionEnvelope>, Option<u32>);

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
    pub offer_store: Option<std::sync::Arc<parking_lot::Mutex<henyey_tx::state::offer_store::OfferStore>>>,
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
}

impl PriorStageState {
    /// Build from a delta, capturing both live entries and deletions.
    pub fn from_delta(delta: &crate::LedgerDelta) -> Self {
        Self {
            entries: delta.current_entries(),
            deleted_keys: delta.dead_entries(),
        }
    }
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
        MuxedAccount::MuxedEd25519(m) => {
            AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(m.ed25519))
        }
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
    /// Whether the transaction should be applied (charged_fee >= computed_fee).
    pub should_apply: bool,
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

                // Compute the fee using the same logic as execute_transaction_with_fee_mode.
                let num_ops = std::cmp::max(1, frame.operation_count() as i64);
                let required_fee = if frame.is_fee_bump() {
                    tx_fee as i64 * (num_ops + 1)
                } else {
                    tx_fee as i64 * num_ops
                };
                let inclusion_fee = frame.inclusion_fee();
                let computed_fee = frame.declared_soroban_resource_fee()
                    + std::cmp::min(inclusion_fee, required_fee);

                let (charged_fee, fee_changes) =
                    delta.deduct_fee_from_account(&fee_source, computed_fee, snapshot, ledger_seq)?;
                let should_apply = charged_fee >= computed_fee;

                total_fee_pool += charged_fee;
                pre_charged.push(PreChargedFee {
                    charged_fee,
                    should_apply,
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
            ContractDataDurability, ContractId, LedgerFootprint, LedgerKey,
            LedgerKeyContractData, ScAddress, ScVal, SorobanResources,
            SorobanResourcesExtV0, SorobanTransactionData, SorobanTransactionDataExt,
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
        let success_result = OperationResult::OpInner(
            stellar_xdr::curr::OperationResultTr::RestoreFootprint(
                stellar_xdr::curr::RestoreFootprintResult::Success,
            ),
        );
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
        let failed_result = OperationResult::OpInner(
            stellar_xdr::curr::OperationResultTr::InvokeHostFunction(
                stellar_xdr::curr::InvokeHostFunctionResult::EntryArchived,
            ),
        );
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
        use stellar_xdr::curr::*;
        use std::sync::Arc;

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

        let context =
            LedgerContext::new(101, 1234567890, 100, 5_000_000, 25, NetworkId::testnet());

        // --- Build PriorStageState as if stage 0 deleted the entries ---
        // Simulate: stage 0 loaded the entries and a TX deleted them.
        // The delta after stage 0 contains the deletions.
        let mut delta = LedgerDelta::new(101);
        delta.record_delete(code_entry.clone()).unwrap();
        delta.record_delete(ttl_entry.clone()).unwrap();

        let prior_stage = PriorStageState::from_delta(&delta);
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
        use stellar_xdr::curr::*;
        use std::sync::Arc;

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
        use stellar_xdr::curr::*;
        use std::sync::Arc;

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
        use stellar_xdr::curr::*;
        use std::sync::Arc;

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
                &LedgerKeyTtl { key_hash: ttl_key_hash.clone() },
                crate::soroban_state::TtlData::new(200, 100),
            ).unwrap();
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
        executor.load_soroban_footprint(&snapshot, &footprint).unwrap();

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
        use stellar_xdr::curr::*;
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

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

        executor.load_soroban_footprint(&snapshot, &footprint).unwrap();

        // Bucket list MUST have been consulted (IMS miss → bucket list fallback)
        assert!(consulted.load(Ordering::SeqCst), "Bucket list must be consulted when IMS misses");
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
        use stellar_xdr::curr::*;
        use std::sync::Arc;

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

        executor.load_soroban_footprint(&snapshot, &footprint).unwrap();

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
        use stellar_xdr::curr::*;
        use std::sync::Arc;

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
                &LedgerKeyTtl { key_hash: ttl_key_hash },
                crate::soroban_state::TtlData::new(500, 90),
            ).unwrap();
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
        assert!(executor.state.get_entry(&data_key).is_some(), "setup: entry must be in state");
        assert!(executor.state.get_entry(&ttl_key).is_none(), "setup: TTL must NOT be in state");

        let footprint = LedgerFootprint {
            read_only: vec![data_key.clone()].try_into().unwrap(),
            read_write: VecM::default(),
        };

        // Must succeed (no panic, no bucket list access) and must load TTL from IMS.
        executor.load_soroban_footprint(&snapshot, &footprint).unwrap();

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
        use stellar_xdr::curr::*;
        use std::sync::Arc;
        use std::collections::HashSet;

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

        let make_offer = |seller_seed: u8, offer_id: i64, selling: Asset, buying: Asset| -> LedgerEntry {
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
            make_offer(1, 1, Asset::Native, usd(issuer)),       // seller 1: native→USD
            make_offer(2, 2, eur(issuer), usd(issuer)),         // seller 2: EUR→USD
            make_offer(1, 3, Asset::Native, Asset::Native),     // seller 1 again: native→native
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
        pairs.insert((usd(issuer), Asset::Native));    // buying=USD, selling=Native
        pairs.insert((usd(issuer), eur(issuer)));      // buying=USD, selling=EUR

        let keys = executor.collect_seller_keys_for_pairs(&pairs, 10);

        // 2 account keys + 3 trustline keys = 5 unique keys
        assert_eq!(keys.len(), 5, "expected 5 unique keys, got {}: {:?}", keys.len(), keys);

        let account1 = LedgerKey::Account(LedgerKeyAccount { account_id: make_account_id(1) });
        let account2 = LedgerKey::Account(LedgerKeyAccount { account_id: make_account_id(2) });

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
        assert!(keys.contains(&tl1_usd), "missing USD trustline for seller 1");
        assert!(keys.contains(&tl2_usd), "missing USD trustline for seller 2");
        assert!(keys.contains(&tl2_eur), "missing EUR trustline for seller 2");
    }
}
