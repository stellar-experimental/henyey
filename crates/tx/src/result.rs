//! Transaction and operation result types.
//!
//! This module provides wrapper types around XDR result structures for easier
//! handling and inspection. The wrappers add convenience methods while preserving
//! access to the underlying XDR data.
//!
//! # Key Types
//!
//! - [`TxApplyResult`]: Result of applying a single transaction, including success
//!   status, fee charged, and detailed result.
//!
//! - [`TxResultWrapper`]: Wrapper around XDR `TransactionResult` with helper methods
//!   for checking success, extracting operation results, and result codes.
//!
//! - [`OpResultWrapper`]: Wrapper around XDR `OperationResult` for checking operation
//!   success and extracting result codes.
//!
//! - [`TxSetResultSummary`]: Aggregates statistics across multiple transactions for
//!   reporting on transaction set application.
//!
//! # Result Codes
//!
//! The [`TxResultCode`] and [`OpResultCode`] enums provide typed access to result
//! codes with human-readable names matching the Stellar documentation.

use stellar_xdr::curr::{
    InnerTransactionResultResult, OperationResult, OperationResultTr, TransactionResult,
    TransactionResultCode, TransactionResultResult,
};

/// Result of applying a transaction.
#[derive(Debug, Clone)]
pub struct TxApplyResult {
    /// Whether the transaction succeeded.
    pub success: bool,
    /// Fee charged (in stroops).
    pub fee_charged: i64,
    /// The transaction result.
    pub result: TxResultWrapper,
}

impl TxApplyResult {
    /// Create a successful result.
    pub fn success(fee_charged: i64, result: TxResultWrapper) -> Self {
        Self {
            success: true,
            fee_charged,
            result,
        }
    }

    /// Create a failed result.
    pub fn failure(fee_charged: i64, result: TxResultWrapper) -> Self {
        Self {
            success: false,
            fee_charged,
            result,
        }
    }
}

/// Wrapper around TransactionResult for easier inspection.
#[derive(Debug, Clone)]
pub struct TxResultWrapper {
    inner: TransactionResult,
}

impl TxResultWrapper {
    /// Create from XDR TransactionResult.
    pub fn from_xdr(result: TransactionResult) -> Self {
        Self { inner: result }
    }

    /// Create a success result.
    pub fn success() -> Self {
        Self {
            inner: TransactionResult {
                fee_charged: 0,
                result: TransactionResultResult::TxSuccess(vec![].try_into().unwrap()),
                ext: stellar_xdr::curr::TransactionResultExt::V0,
            },
        }
    }

    /// Create a fee error result.
    pub fn fee_error() -> Self {
        Self {
            inner: TransactionResult {
                fee_charged: 0,
                result: TransactionResultResult::TxInsufficientFee,
                ext: stellar_xdr::curr::TransactionResultExt::V0,
            },
        }
    }

    /// Create a time bounds error result.
    pub fn time_bounds_error() -> Self {
        Self {
            inner: TransactionResult {
                fee_charged: 0,
                result: TransactionResultResult::TxTooLate,
                ext: stellar_xdr::curr::TransactionResultExt::V0,
            },
        }
    }

    /// Create a no account error result.
    pub fn no_account_error() -> Self {
        Self {
            inner: TransactionResult {
                fee_charged: 0,
                result: TransactionResultResult::TxNoAccount,
                ext: stellar_xdr::curr::TransactionResultExt::V0,
            },
        }
    }

    /// Create a bad sequence error result.
    pub fn bad_seq_error() -> Self {
        Self {
            inner: TransactionResult {
                fee_charged: 0,
                result: TransactionResultResult::TxBadSeq,
                ext: stellar_xdr::curr::TransactionResultExt::V0,
            },
        }
    }

    /// Create an insufficient balance error result.
    pub fn insufficient_balance_error() -> Self {
        Self {
            inner: TransactionResult {
                fee_charged: 0,
                result: TransactionResultResult::TxInsufficientBalance,
                ext: stellar_xdr::curr::TransactionResultExt::V0,
            },
        }
    }

    /// Create an operation failed result.
    pub fn operation_failed() -> Self {
        Self {
            inner: TransactionResult {
                fee_charged: 0,
                result: TransactionResultResult::TxFailed(vec![].try_into().unwrap()),
                ext: stellar_xdr::curr::TransactionResultExt::V0,
            },
        }
    }

    /// Get the underlying XDR result.
    pub fn into_xdr(self) -> TransactionResult {
        self.inner
    }

    /// Get a reference to the underlying XDR result.
    pub fn as_xdr(&self) -> &TransactionResult {
        &self.inner
    }

    /// Get the fee charged.
    pub fn fee_charged(&self) -> i64 {
        self.inner.fee_charged
    }

    /// Check if the transaction succeeded.
    pub fn is_success(&self) -> bool {
        matches!(
            &self.inner.result,
            TransactionResultResult::TxSuccess(_)
                | TransactionResultResult::TxFeeBumpInnerSuccess(_)
        )
    }

    /// Check if the transaction failed.
    pub fn is_failure(&self) -> bool {
        !self.is_success()
    }

    /// Get the result code.
    pub fn result_code(&self) -> TxResultCode {
        self.inner.result.discriminant()
    }

    /// Get the operation results if the transaction was executed.
    pub fn operation_results(&self) -> Option<Vec<OpResultWrapper>> {
        match &self.inner.result {
            TransactionResultResult::TxSuccess(results)
            | TransactionResultResult::TxFailed(results) => Some(
                results
                    .iter()
                    .map(|r| OpResultWrapper::from_xdr(r.clone()))
                    .collect(),
            ),
            TransactionResultResult::TxFeeBumpInnerSuccess(inner)
            | TransactionResultResult::TxFeeBumpInnerFailed(inner) => match &inner.result.result {
                InnerTransactionResultResult::TxSuccess(results)
                | InnerTransactionResultResult::TxFailed(results) => Some(
                    results
                        .iter()
                        .map(|r| OpResultWrapper::from_xdr(r.clone()))
                        .collect(),
                ),
                _ => None,
            },
            _ => None,
        }
    }

    /// Get the number of operations that succeeded.
    pub fn successful_operation_count(&self) -> usize {
        self.operation_results()
            .map(|results| results.iter().filter(|r| r.is_success()).count())
            .unwrap_or(0)
    }

    /// Get the number of operations that failed.
    pub fn failed_operation_count(&self) -> usize {
        self.operation_results()
            .map(|results| results.iter().filter(|r| !r.is_success()).count())
            .unwrap_or(0)
    }
}

/// Type alias: `TxResultCode` is now `TransactionResultCode` from the XDR crate.
///
/// All variants (TxSuccess, TxFailed, TxTooEarly, etc.) are identical.
/// Extension methods are available via [`TransactionResultCodeExt`].
pub type TxResultCode = TransactionResultCode;

/// Extension trait adding convenience methods to `TransactionResultCode`.
pub trait TransactionResultCodeExt {
    /// Check if this is a success code.
    fn is_success(&self) -> bool;

    /// Convert to the XDR `TransactionResultResult` discriminant.
    ///
    /// For error codes (unit variants), this produces a zero-payload result.
    /// For success/failure codes that carry operation results, this produces
    /// an empty operation result vector.
    /// Fee-bump inner codes fall back to `TxInternalError` since they require
    /// an `InnerTransactionResultPair` that cannot be synthesized here.
    fn to_xdr_result(&self) -> TransactionResultResult;
}

impl TransactionResultCodeExt for TransactionResultCode {
    fn is_success(&self) -> bool {
        matches!(
            self,
            TransactionResultCode::TxSuccess | TransactionResultCode::TxFeeBumpInnerSuccess
        )
    }

    fn to_xdr_result(&self) -> TransactionResultResult {
        use TransactionResultCode::*;
        match self {
            TxFeeBumpInnerSuccess | TxFeeBumpInnerFailed => {
                // Fee bump inner results require an InnerTransactionResultPair
                // which we cannot synthesize without the inner tx. Fall back to
                // TxInternalError as a safe default.
                TransactionResultResult::TxInternalError
            }
            TxSuccess => TransactionResultResult::TxSuccess(Vec::new().try_into().unwrap()),
            TxFailed => TransactionResultResult::TxFailed(Vec::new().try_into().unwrap()),
            TxTooEarly => TransactionResultResult::TxTooEarly,
            TxTooLate => TransactionResultResult::TxTooLate,
            TxMissingOperation => TransactionResultResult::TxMissingOperation,
            TxBadSeq => TransactionResultResult::TxBadSeq,
            TxBadAuth => TransactionResultResult::TxBadAuth,
            TxInsufficientBalance => TransactionResultResult::TxInsufficientBalance,
            TxNoAccount => TransactionResultResult::TxNoAccount,
            TxInsufficientFee => TransactionResultResult::TxInsufficientFee,
            TxBadAuthExtra => TransactionResultResult::TxBadAuthExtra,
            TxInternalError => TransactionResultResult::TxInternalError,
            TxNotSupported => TransactionResultResult::TxNotSupported,
            TxBadSponsorship => TransactionResultResult::TxBadSponsorship,
            TxBadMinSeqAgeOrGap => TransactionResultResult::TxBadMinSeqAgeOrGap,
            TxMalformed => TransactionResultResult::TxMalformed,
            TxSorobanInvalid => TransactionResultResult::TxSorobanInvalid,
        }
    }
}

/// Wrapper around OperationResult for easier inspection.
#[derive(Debug, Clone)]
pub struct OpResultWrapper {
    inner: OperationResult,
}

impl OpResultWrapper {
    /// Create from XDR OperationResult.
    pub fn from_xdr(result: OperationResult) -> Self {
        Self { inner: result }
    }

    /// Get the underlying XDR result.
    pub fn into_xdr(self) -> OperationResult {
        self.inner
    }

    /// Get a reference to the underlying XDR result.
    pub fn as_xdr(&self) -> &OperationResult {
        &self.inner
    }

    /// Check if the operation succeeded.
    pub fn is_success(&self) -> bool {
        match &self.inner {
            OperationResult::OpInner(tr) => self.is_tr_success(tr),
            _ => false,
        }
    }

    /// Check if the inner result is a success.
    fn is_tr_success(&self, tr: &OperationResultTr) -> bool {
        match tr {
            OperationResultTr::CreateAccount(r) => {
                matches!(r, stellar_xdr::curr::CreateAccountResult::Success)
            }
            OperationResultTr::Payment(r) => {
                matches!(r, stellar_xdr::curr::PaymentResult::Success)
            }
            OperationResultTr::PathPaymentStrictReceive(r) => {
                matches!(
                    r,
                    stellar_xdr::curr::PathPaymentStrictReceiveResult::Success(_)
                )
            }
            OperationResultTr::ManageSellOffer(r) => {
                matches!(r, stellar_xdr::curr::ManageSellOfferResult::Success(_))
            }
            OperationResultTr::CreatePassiveSellOffer(r) => {
                matches!(r, stellar_xdr::curr::ManageSellOfferResult::Success(_))
            }
            OperationResultTr::SetOptions(r) => {
                matches!(r, stellar_xdr::curr::SetOptionsResult::Success)
            }
            OperationResultTr::ChangeTrust(r) => {
                matches!(r, stellar_xdr::curr::ChangeTrustResult::Success)
            }
            OperationResultTr::AllowTrust(r) => {
                matches!(r, stellar_xdr::curr::AllowTrustResult::Success)
            }
            OperationResultTr::AccountMerge(r) => {
                matches!(r, stellar_xdr::curr::AccountMergeResult::Success(_))
            }
            OperationResultTr::Inflation(r) => {
                matches!(r, stellar_xdr::curr::InflationResult::Success(_))
            }
            OperationResultTr::ManageData(r) => {
                matches!(r, stellar_xdr::curr::ManageDataResult::Success)
            }
            OperationResultTr::BumpSequence(r) => {
                matches!(r, stellar_xdr::curr::BumpSequenceResult::Success)
            }
            OperationResultTr::ManageBuyOffer(r) => {
                matches!(r, stellar_xdr::curr::ManageBuyOfferResult::Success(_))
            }
            OperationResultTr::PathPaymentStrictSend(r) => {
                matches!(
                    r,
                    stellar_xdr::curr::PathPaymentStrictSendResult::Success(_)
                )
            }
            OperationResultTr::CreateClaimableBalance(r) => {
                matches!(
                    r,
                    stellar_xdr::curr::CreateClaimableBalanceResult::Success(_)
                )
            }
            OperationResultTr::ClaimClaimableBalance(r) => {
                matches!(r, stellar_xdr::curr::ClaimClaimableBalanceResult::Success)
            }
            OperationResultTr::BeginSponsoringFutureReserves(r) => {
                matches!(
                    r,
                    stellar_xdr::curr::BeginSponsoringFutureReservesResult::Success
                )
            }
            OperationResultTr::EndSponsoringFutureReserves(r) => {
                matches!(
                    r,
                    stellar_xdr::curr::EndSponsoringFutureReservesResult::Success
                )
            }
            OperationResultTr::RevokeSponsorship(r) => {
                matches!(r, stellar_xdr::curr::RevokeSponsorshipResult::Success)
            }
            OperationResultTr::Clawback(r) => {
                matches!(r, stellar_xdr::curr::ClawbackResult::Success)
            }
            OperationResultTr::ClawbackClaimableBalance(r) => {
                matches!(
                    r,
                    stellar_xdr::curr::ClawbackClaimableBalanceResult::Success
                )
            }
            OperationResultTr::SetTrustLineFlags(r) => {
                matches!(r, stellar_xdr::curr::SetTrustLineFlagsResult::Success)
            }
            OperationResultTr::LiquidityPoolDeposit(r) => {
                matches!(r, stellar_xdr::curr::LiquidityPoolDepositResult::Success)
            }
            OperationResultTr::LiquidityPoolWithdraw(r) => {
                matches!(r, stellar_xdr::curr::LiquidityPoolWithdrawResult::Success)
            }
            OperationResultTr::InvokeHostFunction(r) => {
                matches!(r, stellar_xdr::curr::InvokeHostFunctionResult::Success(_))
            }
            OperationResultTr::ExtendFootprintTtl(r) => {
                matches!(r, stellar_xdr::curr::ExtendFootprintTtlResult::Success)
            }
            OperationResultTr::RestoreFootprint(r) => {
                matches!(r, stellar_xdr::curr::RestoreFootprintResult::Success)
            }
        }
    }

    /// Get the result code.
    pub fn result_code(&self) -> OpResultCode {
        match &self.inner {
            OperationResult::OpInner(_) => OpResultCode::OpInner,
            OperationResult::OpBadAuth => OpResultCode::OpBadAuth,
            OperationResult::OpNoAccount => OpResultCode::OpNoAccount,
            OperationResult::OpNotSupported => OpResultCode::OpNotSupported,
            OperationResult::OpTooManySubentries => OpResultCode::OpTooManySubentries,
            OperationResult::OpExceededWorkLimit => OpResultCode::OpExceededWorkLimit,
            OperationResult::OpTooManySponsoring => OpResultCode::OpTooManySponsoring,
        }
    }
}

/// Operation result codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpResultCode {
    OpInner,
    OpBadAuth,
    OpNoAccount,
    OpNotSupported,
    OpTooManySubentries,
    OpExceededWorkLimit,
    OpTooManySponsoring,
}

impl OpResultCode {
    /// Get a human-readable name.
    pub fn name(&self) -> &'static str {
        match self {
            OpResultCode::OpInner => "opInner",
            OpResultCode::OpBadAuth => "opBadAuth",
            OpResultCode::OpNoAccount => "opNoAccount",
            OpResultCode::OpNotSupported => "opNotSupported",
            OpResultCode::OpTooManySubentries => "opTooManySubentries",
            OpResultCode::OpExceededWorkLimit => "opExceededWorkLimit",
            OpResultCode::OpTooManySponsoring => "opTooManySponsoring",
        }
    }
}

impl std::fmt::Display for OpResultCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

// ============================================================================
// Mutable Transaction Result Types (for live execution)
// ============================================================================

/// Tracks refundable resources and fees for Soroban transactions.
///
/// During Soroban transaction execution, various resources are consumed (events,
/// rent fees, etc.) that may be partially refundable if not fully used. This
/// tracker accumulates consumption and calculates the final refund amount.
///
/// # Example
///
/// ```ignore
/// let mut tracker = RefundableFeeTracker::new(1000);
///
/// // During execution, consume resources
/// tracker.consume_rent_fee(100);
/// tracker.consume_events_size(50);
///
/// // Calculate refund
/// let refund = tracker.get_fee_refund(); // max - consumed
/// ```
#[derive(Debug, Clone)]
pub struct RefundableFeeTracker {
    /// Maximum refundable fee (from transaction).
    max_refundable_fee: i64,
    /// Consumed contract events size in bytes.
    consumed_events_size_bytes: u32,
    /// Consumed rent fee.
    consumed_rent_fee: i64,
    /// Total consumed refundable fee.
    consumed_refundable_fee: i64,
}

impl RefundableFeeTracker {
    /// Create a new tracker with the given maximum refundable fee.
    pub fn new(max_refundable_fee: i64) -> Self {
        Self {
            max_refundable_fee,
            consumed_events_size_bytes: 0,
            consumed_rent_fee: 0,
            consumed_refundable_fee: 0,
        }
    }

    /// Consume rent fee from the refundable budget.
    ///
    /// Returns `Ok(())` if within budget, `Err` if rent fee exceeds available.
    pub fn consume_rent_fee(&mut self, rent_fee: i64) -> Result<(), RefundableFeeError> {
        self.consumed_rent_fee += rent_fee;

        if self.max_refundable_fee < self.consumed_rent_fee {
            return Err(RefundableFeeError::RentFeeExceeded {
                consumed: self.consumed_rent_fee,
                max: self.max_refundable_fee,
            });
        }

        // Update total consumed
        self.consumed_refundable_fee = self.consumed_rent_fee;
        Ok(())
    }

    /// Consume contract events size.
    ///
    /// This is tracked separately and factored into the refundable fee calculation.
    pub fn consume_events_size(&mut self, size_bytes: u32) {
        self.consumed_events_size_bytes += size_bytes;
    }

    /// Update the total consumed refundable fee based on a computed value.
    ///
    /// This is called after computing the actual resource fee based on consumption.
    ///
    /// Returns `Ok(())` if within budget, `Err` if total exceeds maximum.
    pub fn update_consumed_refundable_fee(
        &mut self,
        refundable_fee: i64,
    ) -> Result<(), RefundableFeeError> {
        self.consumed_refundable_fee = self.consumed_rent_fee + refundable_fee;

        if self.max_refundable_fee < self.consumed_refundable_fee {
            return Err(RefundableFeeError::RefundableFeeExceeded {
                consumed: self.consumed_refundable_fee,
                max: self.max_refundable_fee,
            });
        }

        Ok(())
    }

    /// Get the fee refund (max - consumed).
    ///
    /// This is the amount that should be credited back to the fee source account.
    pub fn get_fee_refund(&self) -> i64 {
        self.max_refundable_fee - self.consumed_refundable_fee
    }

    /// Get the maximum refundable fee.
    pub fn max_refundable_fee(&self) -> i64 {
        self.max_refundable_fee
    }

    /// Get the consumed rent fee.
    pub fn consumed_rent_fee(&self) -> i64 {
        self.consumed_rent_fee
    }

    /// Get the total consumed refundable fee.
    pub fn consumed_refundable_fee(&self) -> i64 {
        self.consumed_refundable_fee
    }

    /// Get the consumed events size in bytes.
    pub fn consumed_events_size_bytes(&self) -> u32 {
        self.consumed_events_size_bytes
    }

    /// Reset all consumed fees to 0 (for error cases).
    ///
    /// When a transaction fails, all consumed fees are reset so that the
    /// maximum refund is returned to the fee source.
    pub fn reset_consumed_fee(&mut self) {
        self.consumed_events_size_bytes = 0;
        self.consumed_rent_fee = 0;
        self.consumed_refundable_fee = 0;
    }
}

/// Error type for refundable fee tracking.
#[derive(Debug, Clone)]
pub enum RefundableFeeError {
    /// Rent fee consumption exceeded the available refundable limit.
    RentFeeExceeded { consumed: i64, max: i64 },
    /// Total refundable fee consumption exceeded the available limit.
    RefundableFeeExceeded { consumed: i64, max: i64 },
}

impl std::fmt::Display for RefundableFeeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RentFeeExceeded { consumed, max } => {
                write!(f, "rent fee {} exceeded refundable limit {}", consumed, max)
            }
            Self::RefundableFeeExceeded { consumed, max } => {
                write!(f, "refundable fee {} exceeded limit {}", consumed, max)
            }
        }
    }
}

impl std::error::Error for RefundableFeeError {}

/// Map a `TransactionResultCode` to the corresponding `TransactionResultResult`.
///
/// Delegates to [`TransactionResultCodeExt::to_xdr_result`].
fn code_to_result(code: stellar_xdr::curr::TransactionResultCode) -> TransactionResultResult {
    code.to_xdr_result()
}

/// Mutable transaction result for use during transaction execution.
///
/// This wrapper allows modifying the result as the transaction progresses,
/// including setting error codes and managing refundable fee tracking.
///
/// # Usage
///
/// ```ignore
/// // Create a result for execution
/// let mut result = MutableTransactionResult::new(fee_charged);
///
/// // Initialize refundable fee tracking for Soroban
/// result.initialize_refundable_fee_tracker(max_refundable_fee);
///
/// // Set operation results as they complete
/// result.set_operation_result(0, op_result);
///
/// // On error, set error code (resets refundable fees)
/// result.set_error(TransactionResultCode::TxFailed);
///
/// // Finalize and extract the XDR result
/// result.finalize_fee_refund(protocol_version);
/// let xdr_result = result.into_xdr();
/// ```
#[derive(Debug, Clone)]
pub struct MutableTransactionResult {
    /// The underlying XDR result being built.
    inner: TransactionResult,
    /// Optional refundable fee tracker for Soroban transactions.
    refundable_fee_tracker: Option<RefundableFeeTracker>,
}

impl MutableTransactionResult {
    /// Create a new mutable result with the given fee charged.
    pub fn new(fee_charged: i64) -> Self {
        Self {
            inner: TransactionResult {
                fee_charged,
                result: TransactionResultResult::TxSuccess(vec![].try_into().unwrap()),
                ext: stellar_xdr::curr::TransactionResultExt::V0,
            },
            refundable_fee_tracker: None,
        }
    }

    /// Create a new error result with the given code.
    pub fn create_error(code: stellar_xdr::curr::TransactionResultCode, fee_charged: i64) -> Self {
        use stellar_xdr::curr::TransactionResultCode::*;

        // Fee-bump codes need special handling with InnerTransactionResultPair.
        let result = match code {
            TxFeeBumpInnerSuccess => TransactionResultResult::TxFeeBumpInnerSuccess(
                stellar_xdr::curr::InnerTransactionResultPair {
                    transaction_hash: stellar_xdr::curr::Hash([0u8; 32]),
                    result: stellar_xdr::curr::InnerTransactionResult {
                        fee_charged: 0,
                        result: stellar_xdr::curr::InnerTransactionResultResult::TxSuccess(
                            vec![].try_into().unwrap(),
                        ),
                        ext: stellar_xdr::curr::InnerTransactionResultExt::V0,
                    },
                },
            ),
            TxFeeBumpInnerFailed => TransactionResultResult::TxFeeBumpInnerFailed(
                stellar_xdr::curr::InnerTransactionResultPair {
                    transaction_hash: stellar_xdr::curr::Hash([0u8; 32]),
                    result: stellar_xdr::curr::InnerTransactionResult {
                        fee_charged: 0,
                        result: stellar_xdr::curr::InnerTransactionResultResult::TxFailed(
                            vec![].try_into().unwrap(),
                        ),
                        ext: stellar_xdr::curr::InnerTransactionResultExt::V0,
                    },
                },
            ),
            other => code_to_result(other),
        };

        Self {
            inner: TransactionResult {
                fee_charged,
                result,
                ext: stellar_xdr::curr::TransactionResultExt::V0,
            },
            refundable_fee_tracker: None,
        }
    }

    /// Create a new success result with preallocated operation results.
    pub fn create_success(fee_charged: i64, op_count: usize) -> Self {
        let results = vec![
            OperationResult::OpInner(OperationResultTr::Payment(
                stellar_xdr::curr::PaymentResult::Success,
            ));
            op_count
        ];

        Self {
            inner: TransactionResult {
                fee_charged,
                result: TransactionResultResult::TxSuccess(results.try_into().unwrap_or_default()),
                ext: stellar_xdr::curr::TransactionResultExt::V0,
            },
            refundable_fee_tracker: None,
        }
    }

    /// Set an error code on this result.
    ///
    /// This also resets any consumed refundable fees (for Soroban) so that
    /// the maximum refund is returned to the fee source.
    pub fn set_error(&mut self, code: stellar_xdr::curr::TransactionResultCode) {
        self.inner.result = code_to_result(code);

        // Reset refundable fees on error
        if let Some(ref mut tracker) = self.refundable_fee_tracker {
            tracker.reset_consumed_fee();
        }
    }

    /// Initialize refundable fee tracker for Soroban transactions.
    pub fn initialize_refundable_fee_tracker(&mut self, max_refundable_fee: i64) {
        self.refundable_fee_tracker = Some(RefundableFeeTracker::new(max_refundable_fee));
    }

    /// Get a mutable reference to the refundable fee tracker.
    pub fn refundable_fee_tracker_mut(&mut self) -> Option<&mut RefundableFeeTracker> {
        self.refundable_fee_tracker.as_mut()
    }

    /// Get a reference to the refundable fee tracker.
    pub fn refundable_fee_tracker(&self) -> Option<&RefundableFeeTracker> {
        self.refundable_fee_tracker.as_ref()
    }

    /// Finalize the fee refund and update fee_charged.
    ///
    /// Should be called after transaction execution completes. This applies
    /// the refund (if any) to reduce the fee_charged.
    pub fn finalize_fee_refund(&mut self, _protocol_version: u32) {
        if let Some(ref tracker) = self.refundable_fee_tracker {
            self.inner.fee_charged -= tracker.get_fee_refund();
        }
    }

    /// Check if this result represents success.
    pub fn is_success(&self) -> bool {
        matches!(
            self.inner.result,
            TransactionResultResult::TxSuccess(_)
                | TransactionResultResult::TxFeeBumpInnerSuccess(_)
        )
    }

    /// Get the result code.
    pub fn result_code(&self) -> TxResultCode {
        TxResultWrapper::from_xdr(self.inner.clone()).result_code()
    }

    /// Get the fee charged.
    pub fn fee_charged(&self) -> i64 {
        self.inner.fee_charged
    }

    /// Set the fee charged.
    pub fn set_fee_charged(&mut self, fee_charged: i64) {
        self.inner.fee_charged = fee_charged;
    }

    /// Consume and return the final XDR result.
    pub fn into_xdr(self) -> TransactionResult {
        self.inner
    }

    /// Get a reference to the underlying XDR result.
    pub fn as_xdr(&self) -> &TransactionResult {
        &self.inner
    }

    /// Convert to a TxResultWrapper.
    pub fn into_wrapper(self) -> TxResultWrapper {
        TxResultWrapper::from_xdr(self.inner)
    }
}

/// Summary of transaction results for a transaction set.
#[derive(Debug, Clone, Default)]
pub struct TxSetResultSummary {
    /// Total transactions.
    pub total: usize,
    /// Successful transactions.
    pub successful: usize,
    /// Failed transactions.
    pub failed: usize,
    /// Total fee charged.
    pub total_fee: i64,
    /// Total operations.
    pub total_operations: usize,
    /// Successful operations.
    pub successful_operations: usize,
}

impl TxSetResultSummary {
    /// Create a new empty summary.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a transaction result to the summary.
    pub fn add(&mut self, result: &TxApplyResult, op_count: usize) {
        self.total += 1;
        self.total_fee += result.fee_charged;
        self.total_operations += op_count;

        if result.success {
            self.successful += 1;
            self.successful_operations += result.result.successful_operation_count();
        } else {
            self.failed += 1;
        }
    }

    /// Get the success rate as a percentage.
    pub fn success_rate(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.successful as f64 / self.total as f64) * 100.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    fn create_success_result() -> TransactionResult {
        TransactionResult {
            fee_charged: 100,
            result: TransactionResultResult::TxSuccess(vec![].try_into().unwrap()),
            ext: TransactionResultExt::V0,
        }
    }

    fn create_failed_result() -> TransactionResult {
        TransactionResult {
            fee_charged: 100,
            result: TransactionResultResult::TxBadSeq,
            ext: TransactionResultExt::V0,
        }
    }

    #[test]
    fn test_tx_result_wrapper_success() {
        let result = create_success_result();
        let wrapper = TxResultWrapper::from_xdr(result);

        assert!(wrapper.is_success());
        assert!(!wrapper.is_failure());
        assert_eq!(wrapper.fee_charged(), 100);
        assert_eq!(wrapper.result_code(), TxResultCode::TxSuccess);
    }

    #[test]
    fn test_tx_result_wrapper_failure() {
        let result = create_failed_result();
        let wrapper = TxResultWrapper::from_xdr(result);

        assert!(!wrapper.is_success());
        assert!(wrapper.is_failure());
        assert_eq!(wrapper.result_code(), TxResultCode::TxBadSeq);
    }

    #[test]
    fn test_tx_apply_result() {
        let result = create_success_result();
        let wrapper = TxResultWrapper::from_xdr(result);

        let apply_result = TxApplyResult::success(100, wrapper);
        assert!(apply_result.success);
        assert_eq!(apply_result.fee_charged, 100);
    }

    #[test]
    fn test_tx_set_result_summary() {
        let mut summary = TxSetResultSummary::new();

        let success_result = TxApplyResult {
            success: true,
            fee_charged: 100,
            result: TxResultWrapper::from_xdr(create_success_result()),
        };

        let failed_result = TxApplyResult {
            success: false,
            fee_charged: 100,
            result: TxResultWrapper::from_xdr(create_failed_result()),
        };

        summary.add(&success_result, 1);
        summary.add(&failed_result, 2);

        assert_eq!(summary.total, 2);
        assert_eq!(summary.successful, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.total_fee, 200);
        assert_eq!(summary.total_operations, 3);
        assert_eq!(summary.success_rate(), 50.0);
    }

    #[test]
    fn test_result_code_names() {
        assert_eq!(TxResultCode::TxSuccess.name(), "TxSuccess");
        assert_eq!(TxResultCode::TxBadSeq.name(), "TxBadSeq");
        assert_eq!(OpResultCode::OpBadAuth.name(), "opBadAuth");
    }

    // RefundableFeeTracker tests
    #[test]
    fn test_refundable_fee_tracker_new() {
        let tracker = RefundableFeeTracker::new(1000);
        assert_eq!(tracker.max_refundable_fee(), 1000);
        assert_eq!(tracker.consumed_rent_fee(), 0);
        assert_eq!(tracker.consumed_refundable_fee(), 0);
        assert_eq!(tracker.get_fee_refund(), 1000);
    }

    #[test]
    fn test_refundable_fee_tracker_consume_rent() {
        let mut tracker = RefundableFeeTracker::new(1000);

        assert!(tracker.consume_rent_fee(100).is_ok());
        assert_eq!(tracker.consumed_rent_fee(), 100);
        assert_eq!(tracker.get_fee_refund(), 900);

        assert!(tracker.consume_rent_fee(200).is_ok());
        assert_eq!(tracker.consumed_rent_fee(), 300);
        assert_eq!(tracker.get_fee_refund(), 700);
    }

    #[test]
    fn test_refundable_fee_tracker_rent_exceeds_max() {
        let mut tracker = RefundableFeeTracker::new(100);

        let result = tracker.consume_rent_fee(200);
        assert!(result.is_err());

        if let Err(RefundableFeeError::RentFeeExceeded { consumed, max }) = result {
            assert_eq!(consumed, 200);
            assert_eq!(max, 100);
        } else {
            panic!("expected RentFeeExceeded error");
        }
    }

    #[test]
    fn test_refundable_fee_tracker_reset() {
        let mut tracker = RefundableFeeTracker::new(1000);

        tracker.consume_rent_fee(100).unwrap();
        tracker.consume_events_size(50);
        assert_eq!(tracker.consumed_rent_fee(), 100);
        assert_eq!(tracker.consumed_events_size_bytes(), 50);

        tracker.reset_consumed_fee();
        assert_eq!(tracker.consumed_rent_fee(), 0);
        assert_eq!(tracker.consumed_events_size_bytes(), 0);
        assert_eq!(tracker.get_fee_refund(), 1000);
    }

    #[test]
    fn test_refundable_fee_tracker_update_consumed() {
        let mut tracker = RefundableFeeTracker::new(1000);

        tracker.consume_rent_fee(200).unwrap();
        assert!(tracker.update_consumed_refundable_fee(300).is_ok());
        assert_eq!(tracker.consumed_refundable_fee(), 500); // rent + refundable
        assert_eq!(tracker.get_fee_refund(), 500);
    }

    // MutableTransactionResult tests
    #[test]
    fn test_mutable_result_new() {
        let result = MutableTransactionResult::new(100);
        assert!(result.is_success());
        assert_eq!(result.fee_charged(), 100);
        assert!(result.refundable_fee_tracker().is_none());
    }

    #[test]
    fn test_mutable_result_set_error() {
        let mut result = MutableTransactionResult::new(100);
        assert!(result.is_success());

        result.set_error(stellar_xdr::curr::TransactionResultCode::TxBadSeq);
        assert!(!result.is_success());
        assert_eq!(result.result_code(), TxResultCode::TxBadSeq);
    }

    #[test]
    fn test_mutable_result_error_resets_refundable_fees() {
        let mut result = MutableTransactionResult::new(1000);
        result.initialize_refundable_fee_tracker(500);

        // Consume some fees
        if let Some(tracker) = result.refundable_fee_tracker_mut() {
            tracker.consume_rent_fee(200).unwrap();
        }
        assert_eq!(
            result.refundable_fee_tracker().unwrap().consumed_rent_fee(),
            200
        );

        // Set error should reset consumed fees
        result.set_error(stellar_xdr::curr::TransactionResultCode::TxFailed);
        assert_eq!(
            result.refundable_fee_tracker().unwrap().consumed_rent_fee(),
            0
        );
        assert_eq!(
            result.refundable_fee_tracker().unwrap().get_fee_refund(),
            500
        );
    }

    #[test]
    fn test_mutable_result_finalize_fee_refund() {
        let mut result = MutableTransactionResult::new(1000);
        result.initialize_refundable_fee_tracker(400);

        // Consume some fees
        if let Some(tracker) = result.refundable_fee_tracker_mut() {
            tracker.consume_rent_fee(100).unwrap();
        }

        // Refund should be 400 - 100 = 300
        result.finalize_fee_refund(21);

        // Fee charged should be reduced by refund
        assert_eq!(result.fee_charged(), 700); // 1000 - 300
    }

    #[test]
    fn test_mutable_result_into_xdr() {
        let result = MutableTransactionResult::new(100);
        let xdr = result.into_xdr();

        assert_eq!(xdr.fee_charged, 100);
        assert!(matches!(xdr.result, TransactionResultResult::TxSuccess(_)));
    }

    #[test]
    fn test_mutable_result_create_success() {
        let result = MutableTransactionResult::create_success(200, 3);
        assert!(result.is_success());
        assert_eq!(result.fee_charged(), 200);
    }

    #[test]
    fn test_mutable_result_create_error() {
        let result = MutableTransactionResult::create_error(
            stellar_xdr::curr::TransactionResultCode::TxNoAccount,
            50,
        );
        assert!(!result.is_success());
        assert_eq!(result.fee_charged(), 50);
        assert_eq!(result.result_code(), TxResultCode::TxNoAccount);
    }

    /// Test TxApplyResult::failure constructor.
    #[test]
    fn test_tx_apply_result_failure() {
        let result = create_failed_result();
        let wrapper = TxResultWrapper::from_xdr(result);

        let apply_result = TxApplyResult::failure(100, wrapper);
        assert!(!apply_result.success);
        assert_eq!(apply_result.fee_charged, 100);
        assert_eq!(apply_result.result.result_code(), TxResultCode::TxBadSeq);
    }

    /// Test TxResultWrapper::success constructor.
    #[test]
    fn test_tx_result_wrapper_success_constructor() {
        let wrapper = TxResultWrapper::success();
        assert!(wrapper.is_success());
        assert_eq!(wrapper.result_code(), TxResultCode::TxSuccess);
    }

    /// Test TxSetResultSummary with empty set.
    #[test]
    fn test_tx_set_result_summary_empty() {
        let summary = TxSetResultSummary::new();
        assert_eq!(summary.total, 0);
        assert_eq!(summary.successful, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.total_fee, 0);
        assert_eq!(summary.total_operations, 0);
        // Empty set has 0% success rate (or could be NaN, but we'll check for 0)
        assert!(summary.success_rate().is_nan() || summary.success_rate() == 0.0);
    }

    /// Test TxSetResultSummary with all successful transactions.
    #[test]
    fn test_tx_set_result_summary_all_success() {
        let mut summary = TxSetResultSummary::new();

        for _ in 0..5 {
            let success_result = TxApplyResult {
                success: true,
                fee_charged: 100,
                result: TxResultWrapper::from_xdr(create_success_result()),
            };
            summary.add(&success_result, 1);
        }

        assert_eq!(summary.total, 5);
        assert_eq!(summary.successful, 5);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.success_rate(), 100.0);
    }

    /// Test TxSetResultSummary with all failed transactions.
    #[test]
    fn test_tx_set_result_summary_all_failed() {
        let mut summary = TxSetResultSummary::new();

        for _ in 0..3 {
            let failed_result = TxApplyResult {
                success: false,
                fee_charged: 50,
                result: TxResultWrapper::from_xdr(create_failed_result()),
            };
            summary.add(&failed_result, 2);
        }

        assert_eq!(summary.total, 3);
        assert_eq!(summary.successful, 0);
        assert_eq!(summary.failed, 3);
        assert_eq!(summary.total_fee, 150);
        assert_eq!(summary.total_operations, 6);
        assert_eq!(summary.success_rate(), 0.0);
    }

    /// Test all TxResultCode variants have names.
    #[test]
    fn test_all_tx_result_code_names() {
        // Test common result codes
        assert!(!TxResultCode::TxSuccess.name().is_empty());
        assert!(!TxResultCode::TxFailed.name().is_empty());
        assert!(!TxResultCode::TxTooEarly.name().is_empty());
        assert!(!TxResultCode::TxTooLate.name().is_empty());
        assert!(!TxResultCode::TxMissingOperation.name().is_empty());
        assert!(!TxResultCode::TxBadSeq.name().is_empty());
        assert!(!TxResultCode::TxNoAccount.name().is_empty());
        assert!(!TxResultCode::TxInsufficientBalance.name().is_empty());
        assert!(!TxResultCode::TxBadAuth.name().is_empty());
        assert!(!TxResultCode::TxBadAuthExtra.name().is_empty());
    }

    /// Test all OpResultCode variants have names.
    #[test]
    fn test_all_op_result_code_names() {
        assert!(!OpResultCode::OpInner.name().is_empty());
        assert!(!OpResultCode::OpBadAuth.name().is_empty());
        assert!(!OpResultCode::OpNoAccount.name().is_empty());
        assert!(!OpResultCode::OpNotSupported.name().is_empty());
        assert!(!OpResultCode::OpTooManySubentries.name().is_empty());
        assert!(!OpResultCode::OpExceededWorkLimit.name().is_empty());
    }

    /// Test RefundableFeeTracker with events size consumption.
    #[test]
    fn test_refundable_fee_tracker_events_size() {
        let mut tracker = RefundableFeeTracker::new(1000);

        tracker.consume_events_size(100);
        assert_eq!(tracker.consumed_events_size_bytes(), 100);

        tracker.consume_events_size(200);
        assert_eq!(tracker.consumed_events_size_bytes(), 300);
    }

    /// Test RefundableFeeTracker exhaustion boundary.
    #[test]
    fn test_refundable_fee_tracker_exact_boundary() {
        let mut tracker = RefundableFeeTracker::new(100);

        // Consuming exactly max should succeed
        assert!(tracker.consume_rent_fee(100).is_ok());
        assert_eq!(tracker.consumed_rent_fee(), 100);
        assert_eq!(tracker.get_fee_refund(), 0);
    }

    /// Test MutableTransactionResult with soroban result.
    #[test]
    fn test_mutable_result_soroban() {
        let mut result = MutableTransactionResult::new(500);
        result.initialize_refundable_fee_tracker(300);

        assert!(result.refundable_fee_tracker().is_some());

        // Consume some refundable fee
        if let Some(tracker) = result.refundable_fee_tracker_mut() {
            tracker.consume_rent_fee(50).unwrap();
            assert!(tracker.update_consumed_refundable_fee(100).is_ok());
        }

        let tracker = result.refundable_fee_tracker().unwrap();
        assert_eq!(tracker.consumed_rent_fee(), 50);
        assert_eq!(tracker.consumed_refundable_fee(), 150); // 50 rent + 100 other
    }

    /// Test TxResultWrapper with inner transaction (fee bump).
    #[test]
    fn test_tx_result_wrapper_fee_bump() {
        let inner_result = TransactionResult {
            fee_charged: 100,
            result: TransactionResultResult::TxFeeBumpInnerSuccess(
                stellar_xdr::curr::InnerTransactionResultPair {
                    transaction_hash: stellar_xdr::curr::Hash([0u8; 32]),
                    result: stellar_xdr::curr::InnerTransactionResult {
                        fee_charged: 50,
                        result: InnerTransactionResultResult::TxSuccess(vec![].try_into().unwrap()),
                        ext: stellar_xdr::curr::InnerTransactionResultExt::V0,
                    },
                },
            ),
            ext: TransactionResultExt::V0,
        };

        let wrapper = TxResultWrapper::from_xdr(inner_result);
        assert!(wrapper.is_success());
        assert_eq!(wrapper.result_code(), TxResultCode::TxFeeBumpInnerSuccess);
    }

    #[test]
    fn test_tx_result_code_to_xdr_result_maps_common_error_codes() {
        assert!(matches!(
            TxResultCode::TxBadSeq.to_xdr_result(),
            TransactionResultResult::TxBadSeq
        ));
        assert!(matches!(
            TxResultCode::TxInsufficientFee.to_xdr_result(),
            TransactionResultResult::TxInsufficientFee
        ));
        assert!(matches!(
            TxResultCode::TxMalformed.to_xdr_result(),
            TransactionResultResult::TxMalformed
        ));
    }

    #[test]
    fn test_tx_result_code_to_xdr_result_maps_success_and_failed_with_empty_ops() {
        match TxResultCode::TxSuccess.to_xdr_result() {
            TransactionResultResult::TxSuccess(ops) => assert!(ops.is_empty()),
            other => panic!("unexpected result for TxSuccess: {:?}", other),
        }

        match TxResultCode::TxFailed.to_xdr_result() {
            TransactionResultResult::TxFailed(ops) => assert!(ops.is_empty()),
            other => panic!("unexpected result for TxFailed: {:?}", other),
        }
    }

    #[test]
    fn test_tx_result_code_to_xdr_result_fee_bump_variants_fallback_to_internal_error() {
        assert!(matches!(
            TxResultCode::TxFeeBumpInnerSuccess.to_xdr_result(),
            TransactionResultResult::TxInternalError
        ));
        assert!(matches!(
            TxResultCode::TxFeeBumpInnerFailed.to_xdr_result(),
            TransactionResultResult::TxInternalError
        ));
    }
}
