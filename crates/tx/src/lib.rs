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
//! use stellar_xdr::{TransactionEnvelope, TransactionResult, TransactionMeta};
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
pub mod fees;
mod frame;
pub mod frozen_keys;
pub(crate) mod live_execution;
pub(crate) mod meta_builder;
pub mod operations;
mod result;
pub(crate) mod scval_utils;
pub(crate) mod signature_checker;
pub mod soroban;
pub mod state;
#[cfg(test)]
pub mod test_utils;
pub mod tx_set_xdr;
pub mod validation;

// Re-export error types
pub use error::TxError;
pub use events::{
    make_account_address, make_claimable_balance_address, make_muxed_account_address,
    ClassicEventConfig, EventManagerHierarchy, OpEventManager, P23SacReconciler,
    SacReconciliationInfo, TxEventManager,
};

// Re-export frame types
pub use frame::{
    envelope_sequence_number, muxed_to_account_id, muxed_to_ed25519, soroban_disk_read_entries,
    TransactionFrame,
};

// Re-export apply types and functions
pub use apply::{apply_from_history, ChangeRef, TxChangeLog};

// Re-export result types
pub use result::{
    MutableTransactionResult, OpResultCode, RefundableFeeError, RefundableFeeTracker,
    TransactionResultCodeExt, TxApplyResult, TxResultCode, TxResultWrapper,
};

// Re-export signature checker types
pub use signature_checker::{collect_signers_for_account, SignatureChecker};

// Re-export validation types and functions
pub use validation::{
    check_valid_pre_seq_num, check_valid_pre_seq_num_with_config, is_too_early, is_too_late,
    validate_basic, validate_fee, validate_full, validate_sequence, validate_signatures,
    validate_structure, verify_signature_with_key, verify_signature_with_raw_key, LedgerContext,
    PreSeqNumError, SorobanResourceLimits, ValidationError,
};

// Re-export operation types
pub use operations::{
    collect_prefetch_keys, get_threshold_level, is_op_supported, malformed_operation_result,
    validate_classic_op_structure, validate_operation, OperationTypeExt, OperationValidationError,
    ThresholdLevel,
};
pub use stellar_xdr::OperationType;

// Re-export state types
pub use state::{AssetPair, LedgerStateManager, OfferDescriptor, OfferIndex, OfferKey};

// Re-export fee bump types
pub use fee_bump::{
    calculate_inner_fee_charged, extract_inner_hash_from_result, fee_bump_refund_applies_to_inner,
    validate_fee_bump, verify_inner_signatures, wrap_inner_result_in_fee_bump, FeeBumpError,
    FeeBumpFrame, FeeBumpMutableTransactionResult,
};

// Re-export fee newtypes
pub use fees::{FeeRate, InclusionFee, ResourceFee, TotalFee};

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

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::*;

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

    /// Test TransactionFrame with fee bump detection.
    #[test]
    fn test_frame_fee_bump_detection() {
        // Regular transaction is not fee bump
        let envelope = create_test_envelope();
        let frame = TransactionFrame::from_owned(envelope);
        assert!(!frame.is_fee_bump());
    }
}
