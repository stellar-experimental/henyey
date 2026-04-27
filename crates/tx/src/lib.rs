//! Transaction processing for rs-stellar-core.
//!
//! This crate provides the core transaction validation and execution logic for
//! the Stellar network, supporting both classic Stellar operations and Soroban
//! smart contract execution.
//!
//! # Overview
//!
//! The crate supports two first-class modes of operation:
//!
//! 1. **Live Execution Mode**: Validates and executes transactions in real-time,
//!    producing deterministic results that match stellar-core. This is the
//!    mode used by validators to close ledgers.
//!
//! 2. **Catchup/Replay Mode**: Applies historical transactions from archives
//!    by trusting the recorded results and replaying state changes. This is used
//!    for fast synchronization with the network.
//!
//! # Key Types
//!
//! - [`TransactionFrame`]: Wrapper around XDR `TransactionEnvelope` providing
//!   convenient access to transaction properties and hash computation.
//!
//! - [`LiveExecutionContext`]: Context for live transaction execution including
//!   ledger state, fee pool tracking, and protocol configuration.
//!
//! - [`TxChangeLog`]: Accumulates all state changes (creates, updates, deletes)
//!   during transaction execution for later persistence.
//!
//! - [`LedgerContext`]: Provides ledger-level context (sequence, close time,
//!   base fee, network ID) needed for validation and execution.
//!
//! - [`LedgerStateManager`]: In-memory ledger state for transaction execution,
//!   with support for snapshots and rollback.
//!
//! # Transaction Workflow (Live Execution Mode)
//!
//! ```ignore
//! use henyey_tx::{
//!     TransactionFrame, LiveExecutionContext, LedgerContext, LedgerStateManager,
//!     process_fee_seq_num, process_post_apply, process_post_tx_set_apply,
//! };
//!
//! // Set up execution context
//! let ledger_ctx = LedgerContext::mainnet(ledger_seq, close_time);
//! let state = LedgerStateManager::new(base_reserve, ledger_seq);
//! let mut ctx = LiveExecutionContext::new(ledger_ctx, state);
//!
//! // Phase 1: Process fees and sequence numbers
//! let fee_result = process_fee_seq_num(&frame, &mut ctx, None)?;
//! let mut tx_result = fee_result.tx_result;
//!
//! // Phase 2: Apply operations (handled by operation modules)
//! // ... apply operations ...
//!
//! // Phase 3: Post-apply (pre-P23 Soroban refunds)
//! process_post_apply(&frame, &mut ctx, &mut tx_result, None)?;
//!
//! // Phase 4: Transaction set post-apply (P23+ Soroban refunds)
//! process_post_tx_set_apply(&frame, &mut ctx, &mut tx_result, None)?;
//! ```
//!
//! # Transaction Workflow (Catchup Mode)
//!
//! ```ignore
//! use henyey_tx::{TransactionFrame, apply_from_history, TxChangeLog};
//! use stellar_xdr::curr::{TransactionEnvelope, TransactionResult, TransactionMeta};
//!
//! // Parse transaction from archive
//! let envelope: TransactionEnvelope = /* from archive */;
//! let result: TransactionResult = /* from archive */;
//! let meta: TransactionMeta = /* from archive */;
//!
//! // Create frame wrapper
//! let frame = TransactionFrame::from_owned(envelope);
//!
//! // Apply historical transaction to accumulate state changes
//! let mut delta = TxChangeLog::new(ledger_seq);
//! let apply_result = apply_from_history(&frame, &result, &meta, &mut delta)?;
//!
//! // Delta now contains all state changes to apply to the bucket list
//! for entry in delta.created_entries() {
//!     // Process created entries
//! }
//! ```
//!
//! # Classic Operations
//!
//! All standard Stellar operations are supported:
//!
//! - **Account**: `CreateAccount`, `AccountMerge`, `SetOptions`, `BumpSequence`
//! - **Payments**: `Payment`, `PathPaymentStrictReceive`, `PathPaymentStrictSend`
//! - **DEX**: `ManageSellOffer`, `ManageBuyOffer`, `CreatePassiveSellOffer`
//! - **Trust**: `ChangeTrust`, `AllowTrust`, `SetTrustLineFlags`
//! - **Data**: `ManageData`
//! - **Claimable Balances**: `CreateClaimableBalance`, `ClaimClaimableBalance`
//! - **Sponsorship**: `BeginSponsoringFutureReserves`, `EndSponsoringFutureReserves`, `RevokeSponsorship`
//! - **Clawback**: `Clawback`, `ClawbackClaimableBalance`
//! - **Liquidity Pools**: `LiquidityPoolDeposit`, `LiquidityPoolWithdraw`
//! - **Deprecated**: `Inflation`
//!
//! # Soroban Operations
//!
//! Smart contract operations with protocol-versioned host integration:
//!
//! - `InvokeHostFunction`: Execute contract functions with full state access
//! - `ExtendFootprintTtl`: Extend the time-to-live of contract state
//! - `RestoreFootprint`: Restore archived contract state from hot archive
//!
//! # Protocol Versioning
//!
//! The crate supports multiple Stellar protocol versions and uses the correct
//! soroban-env-host version for each protocol to ensure deterministic replay.

// ---------------------------------------------------------------------------
// Protocol-level constants
// ---------------------------------------------------------------------------

/// Maximum number of operations allowed in a single transaction.
///
/// Parity: stellar-core `MAX_OPS_PER_TX` (Herder.h).
pub(crate) const MAX_OPS_PER_TX: usize = 100;

/// Network minimum base fee per operation (in stroops).
///
/// Used as the default when no base fee is explicitly provided.
/// Parity: stellar-core `MIN_INCLUSION_FEE` / 1 operation.
pub(crate) const NETWORK_MIN_BASE_FEE: i64 = 100;

mod apply;
pub mod envelope_utils;
mod error;
mod events;
pub(crate) mod fee_bump;
mod frame;
pub mod frozen_keys;
pub(crate) mod live_execution;
pub(crate) mod lumen_reconciler;
pub(crate) mod meta_builder;
pub mod operations;
mod result;
pub(crate) mod scval_utils;
pub(crate) mod signature_checker;
pub mod soroban;
pub mod state;
#[cfg(test)]
pub mod test_utils;
pub mod validation;

// Re-export error types
pub use error::TxError;
pub use events::{
    make_account_address, make_claimable_balance_address, make_muxed_account_address,
    ClassicEventConfig, EventManagerHierarchy, OpEventManager, TxEventManager,
};

// Re-export lumen reconciler types
pub use lumen_reconciler::{reconcile_events, LumenEventReconciler, ReconcilerConfig};

// Re-export frame types
pub use frame::{
    envelope_sequence_number, muxed_to_account_id, muxed_to_ed25519, soroban_disk_read_entries,
    TransactionFrame,
};

// Re-export apply types and functions
pub use apply::{
    apply_fee_only, apply_from_history, apply_transaction_set_from_history, ChangeRef, TxChangeLog,
};

// Re-export result types
pub use result::{
    MutableTransactionResult, OpResultCode, OpResultWrapper, RefundableFeeError,
    RefundableFeeTracker, TransactionResultCodeExt, TxApplyResult, TxResultCode, TxResultWrapper,
    TxSetResultSummary,
};

// Re-export signature checker types
pub use signature_checker::{collect_signers_for_account, SignatureChecker};

// Re-export validation types and functions
pub use validation::{
    check_valid_pre_seq_num, check_valid_pre_seq_num_with_config, validate_basic, validate_fee,
    validate_full, validate_ledger_bounds, validate_sequence, validate_signatures,
    validate_structure, validate_time_bounds, verify_signature_with_key,
    verify_signature_with_raw_key, LedgerContext, PreSeqNumError, ValidationError,
};

// Re-export operation types
pub use operations::{
    collect_prefetch_keys, get_threshold_level, is_op_supported, validate_operation,
    OperationTypeExt, OperationValidationError, ThresholdLevel,
};
pub use stellar_xdr::curr::OperationType;

// Re-export state types
pub use state::{AssetPair, LedgerStateManager, OfferDescriptor, OfferIndex, OfferKey};

// Re-export fee bump types
pub use fee_bump::{
    calculate_inner_fee_charged, extract_inner_hash_from_result, validate_fee_bump,
    verify_inner_signatures, wrap_inner_result_in_fee_bump, FeeBumpError, FeeBumpFrame,
    FeeBumpMutableTransactionResult,
};

// Re-export live execution types
pub use live_execution::{
    apply_transaction, process_fee_seq_num, process_post_apply, process_post_tx_set_apply,
    process_seq_num, refund_soroban_fee, remove_one_time_signers, FeeSeqNumResult,
    LiveExecutionContext,
};

/// Result type alias for transaction operations.
///
/// This is the standard Result type used throughout the crate, with [`TxError`]
/// as the error type.
pub type Result<T> = std::result::Result<T, TxError>;

/// Summary result of transaction validation.
///
/// This enum provides a simplified view of validation outcomes, suitable for
/// quick checks and logging. For detailed error information, use the
/// [`ValidationError`] type returned by validation functions.
///
/// # Mapping from ValidationError
///
/// Each `ValidationResult` variant corresponds to one or more [`ValidationError`]
/// variants. The [`From<ValidationError>`] implementation provides this mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationResult {
    /// Transaction passed all validation checks.
    Valid,
    /// Transaction has invalid or missing signature(s).
    InvalidSignature,
    /// Transaction fee is below the minimum required.
    InsufficientFee,
    /// Source account sequence number does not match expected.
    BadSequence,
    /// Source account does not exist in the ledger.
    NoAccount,
    /// Source account has insufficient balance for the fee.
    InsufficientBalance,
    /// Transaction's max time bound has passed.
    TooLate,
    /// Transaction's min time bound has not yet been reached.
    TooEarly,
    /// Minimum sequence age or ledger gap precondition not met.
    BadMinSeqAgeOrGap,
    /// Extra signer requirements specified in preconditions not met.
    BadAuthExtra,
    /// Other validation failure (structure, fee bump inner tx, etc.).
    Invalid,
}

impl From<ValidationError> for ValidationResult {
    fn from(err: ValidationError) -> Self {
        match err {
            ValidationError::InvalidStructure(_) => ValidationResult::Invalid,
            ValidationError::InvalidSignature => ValidationResult::InvalidSignature,
            ValidationError::MissingSignatures => ValidationResult::InvalidSignature,
            ValidationError::BadSequence { .. } => ValidationResult::BadSequence,
            ValidationError::InsufficientFee { .. } => ValidationResult::InsufficientFee,
            ValidationError::SourceAccountNotFound => ValidationResult::NoAccount,
            ValidationError::InsufficientBalance => ValidationResult::InsufficientBalance,
            ValidationError::TooLate { .. } => ValidationResult::TooLate,
            ValidationError::TooEarly { .. } => ValidationResult::TooEarly,
            ValidationError::LedgerBoundsTooEarly { .. } => ValidationResult::TooEarly,
            ValidationError::LedgerBoundsTooLate { .. } => ValidationResult::TooLate,
            ValidationError::BadMinAccountSequence => ValidationResult::BadSequence,
            ValidationError::BadMinAccountSequenceAge => ValidationResult::BadMinSeqAgeOrGap,
            ValidationError::BadMinAccountSequenceLedgerGap => ValidationResult::BadMinSeqAgeOrGap,
            ValidationError::ExtraSignersNotMet => ValidationResult::BadAuthExtra,
            ValidationError::FeeBumpInsufficientFee { .. } => ValidationResult::InsufficientFee,
            ValidationError::FeeBumpInvalidInner(_) => ValidationResult::Invalid,
        }
    }
}

/// High-level transaction validator.
///
/// Provides a convenient interface for validating transaction envelopes
/// against ledger context and optionally source account data.
///
/// # Example
///
/// ```ignore
/// use henyey_tx::TransactionValidator;
/// use stellar_xdr::curr::TransactionEnvelope;
///
/// let validator = TransactionValidator::testnet(1000, 1625000000);
/// let envelope: TransactionEnvelope = /* ... */;
///
/// match validator.validate(&envelope) {
///     ValidationResult::Valid => println!("Transaction is valid"),
///     ValidationResult::InsufficientFee => println!("Fee too low"),
///     other => println!("Validation failed: {:?}", other),
/// }
/// ```
pub struct TransactionValidator {
    /// Ledger context used for validation.
    context: LedgerContext,
}

impl TransactionValidator {
    /// Create a new validator with the given ledger context.
    pub fn new(context: LedgerContext) -> Self {
        Self { context }
    }

    /// Create a validator for testnet.
    pub fn testnet(sequence: u32, close_time: u64) -> Self {
        Self {
            context: LedgerContext::testnet(sequence, close_time),
        }
    }

    /// Create a validator for mainnet.
    pub fn mainnet(sequence: u32, close_time: u64) -> Self {
        Self {
            context: LedgerContext::mainnet(sequence, close_time),
        }
    }

    /// Validate a transaction envelope (basic checks only).
    pub fn validate(&self, tx: &stellar_xdr::curr::TransactionEnvelope) -> ValidationResult {
        let frame = TransactionFrame::from_owned(tx.clone());

        match validate_basic(&frame, &self.context) {
            Ok(()) => ValidationResult::Valid,
            Err(errors) => {
                // Return the first error
                if let Some(err) = errors.into_iter().next() {
                    err.into()
                } else {
                    ValidationResult::Invalid
                }
            }
        }
    }

    /// Full validation with account data.
    pub fn validate_with_account(
        &self,
        tx: &stellar_xdr::curr::TransactionEnvelope,
        source_account: &stellar_xdr::curr::AccountEntry,
    ) -> ValidationResult {
        let frame = TransactionFrame::from_owned(tx.clone());

        match validate_full(&frame, &self.context, source_account) {
            Ok(()) => ValidationResult::Valid,
            Err(errors) => {
                if let Some(err) = errors.into_iter().next() {
                    err.into()
                } else {
                    ValidationResult::Invalid
                }
            }
        }
    }

    /// Check if all required signatures are present.
    pub fn check_signatures(&self, tx: &stellar_xdr::curr::TransactionEnvelope) -> bool {
        let frame = TransactionFrame::from_owned(tx.clone());
        validate_signatures(&frame, &self.context).is_ok()
    }
}

/// Simplified transaction execution result.
///
/// This is a convenience wrapper that provides easy access to the most
/// commonly needed information from a transaction execution. For full
/// details, use [`TxApplyResult`] and its [`TxResultWrapper`].
///
/// This type can be constructed from [`TxApplyResult`] via the [`From`] trait.
#[derive(Debug, Clone)]
pub struct TransactionResult {
    /// The fee charged in stroops.
    pub fee_charged: i64,
    /// Result of each operation in the transaction.
    pub operation_results: Vec<OperationResult>,
    /// Whether the transaction as a whole succeeded.
    pub success: bool,
}

impl From<TxApplyResult> for TransactionResult {
    fn from(result: TxApplyResult) -> Self {
        Self {
            fee_charged: result.fee_charged,
            operation_results: result
                .result
                .operation_results()
                .map(|ops| {
                    ops.into_iter()
                        .map(|op| {
                            if op.is_success() {
                                OperationResult::Success
                            } else {
                                OperationResult::Failed(OperationError::OpFailed)
                            }
                        })
                        .collect()
                })
                .unwrap_or_default(),
            success: result.success,
        }
    }
}

/// Simplified operation execution result.
///
/// Indicates whether an individual operation within a transaction succeeded
/// or failed. For detailed operation-specific results, use the XDR
/// `OperationResult` type from `stellar_xdr`.
#[derive(Debug, Clone)]
pub enum OperationResult {
    /// Operation completed successfully.
    Success,
    /// Operation failed with an error.
    Failed(OperationError),
}

/// Simplified operation error categories.
///
/// These are high-level error categories that cover the most common failure
/// modes. For detailed error codes, use the XDR `OperationResult` variants.
#[derive(Debug, Clone)]
pub enum OperationError {
    /// Generic operation failure (no specific category).
    OpFailed,
    /// Required account does not exist.
    NoAccount,
    /// Insufficient balance or reserve for the operation.
    Underfunded,
    /// Trustline or offer capacity exceeded.
    LineFull,
    /// Asset authorization check failed.
    NotAuthorized,
    /// Other operation-specific error with description.
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    fn create_test_envelope() -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([1u8; 32]));

        let payment_op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest,
                asset: Asset::Native,
                amount: 1000,
            }),
        };

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![payment_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        })
    }

    #[test]
    fn test_validator_creation() {
        let validator = TransactionValidator::testnet(1, 1000);
        let envelope = create_test_envelope();

        // Basic validation should pass
        let result = validator.validate(&envelope);
        assert_eq!(result, ValidationResult::Valid);
    }

    #[test]
    fn test_frame_creation_and_properties() {
        let envelope = create_test_envelope();
        let frame = TransactionFrame::from_owned(envelope);

        assert_eq!(frame.operation_count(), 1);
        assert_eq!(frame.fee(), 100);
        assert_eq!(frame.sequence_number(), 1);
        assert!(!frame.is_soroban());
        assert!(!frame.is_fee_bump());
    }

    #[test]
    fn test_ledger_delta() {
        let mut delta = TxChangeLog::new(100);

        assert_eq!(delta.ledger_seq(), 100);
        assert!(!delta.has_changes());

        delta.add_fee(500);
        assert_eq!(delta.fee_charged(), 500);
    }

    #[test]
    fn test_operation_type() {
        assert!(OperationType::InvokeHostFunction.is_soroban());
        assert!(!OperationType::Payment.is_soroban());
        assert_eq!(OperationType::Payment.name(), "Payment");
    }

    /// Test all Soroban operation types.
    #[test]
    fn test_soroban_operation_types() {
        assert!(OperationType::InvokeHostFunction.is_soroban());
        assert!(OperationType::ExtendFootprintTtl.is_soroban());
        assert!(OperationType::RestoreFootprint.is_soroban());
    }

    /// Test all classic operation types are not Soroban.
    #[test]
    fn test_classic_operation_types() {
        let classic_ops = [
            OperationType::CreateAccount,
            OperationType::Payment,
            OperationType::PathPaymentStrictReceive,
            OperationType::ManageSellOffer,
            OperationType::CreatePassiveSellOffer,
            OperationType::SetOptions,
            OperationType::ChangeTrust,
            OperationType::AllowTrust,
            OperationType::AccountMerge,
            OperationType::Inflation,
            OperationType::ManageData,
            OperationType::BumpSequence,
            OperationType::ManageBuyOffer,
            OperationType::PathPaymentStrictSend,
            OperationType::CreateClaimableBalance,
            OperationType::ClaimClaimableBalance,
            OperationType::BeginSponsoringFutureReserves,
            OperationType::EndSponsoringFutureReserves,
            OperationType::RevokeSponsorship,
            OperationType::Clawback,
            OperationType::ClawbackClaimableBalance,
            OperationType::SetTrustLineFlags,
            OperationType::LiquidityPoolDeposit,
            OperationType::LiquidityPoolWithdraw,
        ];

        for op in classic_ops {
            assert!(
                !op.is_soroban(),
                "{} should not be a Soroban operation",
                op.name()
            );
        }
    }

    /// Test ValidationResult enum.
    #[test]
    fn test_validation_result() {
        let valid = ValidationResult::Valid;
        assert!(matches!(valid, ValidationResult::Valid));

        let invalid = ValidationResult::Invalid;
        assert!(matches!(invalid, ValidationResult::Invalid));

        let no_account = ValidationResult::NoAccount;
        assert!(matches!(no_account, ValidationResult::NoAccount));

        let bad_seq = ValidationResult::BadSequence;
        assert!(matches!(bad_seq, ValidationResult::BadSequence));

        let insuff_fee = ValidationResult::InsufficientFee;
        assert!(matches!(insuff_fee, ValidationResult::InsufficientFee));

        let invalid_sig = ValidationResult::InvalidSignature;
        assert!(matches!(invalid_sig, ValidationResult::InvalidSignature));
    }

    /// Test TxError variants.
    #[test]
    fn test_tx_error_display() {
        let err = TxError::ValidationFailed("test error".to_string());
        let display = format!("{}", err);
        assert!(display.contains("test error"));

        let err = TxError::InvalidSignature;
        let display = format!("{}", err);
        assert!(display.contains("signature"));

        let err = TxError::SourceAccountNotFound;
        let display = format!("{}", err);
        assert!(display.contains("account"));
    }

    /// Test OperationError variants.
    #[test]
    fn test_operation_error() {
        let err = OperationError::Underfunded;
        assert!(matches!(err, OperationError::Underfunded));

        let err = OperationError::LineFull;
        assert!(matches!(err, OperationError::LineFull));

        let err = OperationError::NotAuthorized;
        assert!(matches!(err, OperationError::NotAuthorized));

        let err = OperationError::NoAccount;
        assert!(matches!(err, OperationError::NoAccount));

        let err = OperationError::OpFailed;
        assert!(matches!(err, OperationError::OpFailed));

        let err = OperationError::Other("custom error".to_string());
        assert!(matches!(err, OperationError::Other(_)));
    }

    /// Test TransactionFrame with fee bump detection.
    #[test]
    fn test_frame_fee_bump_detection() {
        // Regular transaction is not fee bump
        let envelope = create_test_envelope();
        let frame = TransactionFrame::from_owned(envelope);
        assert!(!frame.is_fee_bump());
    }

    /// Test TransactionValidator with different network contexts.
    #[test]
    fn test_validator_networks() {
        let testnet_validator = TransactionValidator::testnet(1, 1000);
        let mainnet_validator = TransactionValidator::mainnet(1, 1000);

        let envelope = create_test_envelope();
        // Both should validate basic transaction structure
        assert_eq!(
            testnet_validator.validate(&envelope),
            ValidationResult::Valid
        );
        assert_eq!(
            mainnet_validator.validate(&envelope),
            ValidationResult::Valid
        );
    }

    /// Regression test: LedgerBoundsTooLate maps to ValidationResult::TooLate
    /// (not Invalid). Covers the equality case current == max_ledger.
    #[test]
    fn test_ledger_bounds_too_late_maps_to_too_late() {
        let result: ValidationResult = ValidationError::LedgerBoundsTooLate {
            max_ledger: 100,
            current: 100,
        }
        .into();
        assert_eq!(result, ValidationResult::TooLate);
    }

    /// Regression test: LedgerBoundsTooEarly maps to ValidationResult::TooEarly.
    #[test]
    fn test_ledger_bounds_too_early_maps_to_too_early() {
        let result: ValidationResult = ValidationError::LedgerBoundsTooEarly {
            min_ledger: 200,
            current: 50,
        }
        .into();
        assert_eq!(result, ValidationResult::TooEarly);
    }
}
