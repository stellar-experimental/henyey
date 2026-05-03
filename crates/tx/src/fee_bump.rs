//! Fee bump transaction handling.
//!
//! This module provides comprehensive support for fee bump transactions (CAP-0015),
//! which allow replacing a transaction's fee with a higher one. Fee bump transactions
//! wrap an existing inner transaction and provide a new fee paid by a potentially
//! different account.
//!
//! # Overview
//!
//! Fee bump transactions consist of:
//! - An outer envelope with the fee source account and new (higher) fee
//! - The original inner transaction with its signatures
//!
//! # Key Features
//!
//! - [`FeeBumpFrame`]: Wrapper providing fee bump-specific accessors
//! - [`FeeBumpMutableTransactionResult`]: Mutable result with inner transaction tracking
//! - [`validate_fee_bump`]: Fee bump-specific validation
//! - Inner transaction hash computation for result reporting
//!
//! # Parity
//!
//! This module mirrors the behavior of stellar-core `FeeBumpTransactionFrame` including:
//! - Outer fee >= inner fee validation (with base fee multiplier)
//! - Inner signature cryptographic verification
//! - Inner transaction hash in result pair
//! - Protocol-versioned fee refund logic
//!
//! # Example
//!
//! ```ignore
//! use henyey_tx::fee_bump::{FeeBumpFrame, validate_fee_bump};
//!
//! let frame = FeeBumpFrame::from_envelope(envelope, &network_id)?;
//!
//! // Validate fee bump specific rules
//! validate_fee_bump(&frame, &context)?;
//!
//! // Get the inner transaction hash for result reporting
//! let inner_hash = frame.inner_transaction_hash()?;
//! ```

use henyey_common::protocol::{protocol_version_starts_from, ProtocolVersion};
use henyey_common::{Hash256, NetworkId};
use stellar_xdr::curr::{
    AccountId, DecoratedSignature, FeeBumpTransaction, FeeBumpTransactionEnvelope,
    FeeBumpTransactionInnerTx, Hash, InnerTransactionResult, InnerTransactionResultExt,
    InnerTransactionResultPair, InnerTransactionResultResult, MuxedAccount, OperationResult,
    TransactionEnvelope, TransactionResult, TransactionResultExt, TransactionResultResult,
    TransactionV1Envelope,
};

use crate::frame::{muxed_to_account_id, TransactionFrame};
use crate::result::{RefundableFeeTracker, TxResultCode, TxResultWrapper};
use crate::validation::LedgerContext;
use crate::Result;

/// Fee bump validation errors.
#[derive(Debug, Clone)]
pub enum FeeBumpError {
    /// Not a fee bump transaction.
    NotFeeBump,
    /// Outer fee is less than inner fee.
    InsufficientOuterFee { outer_fee: i64, required_min: i64 },
    /// Inner transaction has too many operations.
    TooManyOperations(usize),
    /// Inner transaction is not V1.
    InvalidInnerTxType,
    /// Failed to compute hash.
    HashError(String),
}

impl std::fmt::Display for FeeBumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFeeBump => write!(f, "not a fee bump transaction"),
            Self::InsufficientOuterFee {
                outer_fee,
                required_min,
            } => write!(
                f,
                "outer fee {} is less than required minimum {}",
                outer_fee, required_min
            ),
            Self::TooManyOperations(count) => {
                write!(f, "inner transaction has too many operations: {}", count)
            }
            Self::InvalidInnerTxType => write!(f, "inner transaction must be V1"),
            Self::HashError(msg) => write!(f, "hash computation error: {}", msg),
        }
    }
}

impl std::error::Error for FeeBumpError {}

/// Wrapper around a fee bump transaction providing fee bump-specific accessors.
///
/// `FeeBumpFrame` wraps a [`TransactionFrame`] that contains a fee bump transaction
/// and provides convenient methods for accessing both the outer fee bump data and
/// the wrapped inner transaction.
///
/// # Construction
///
/// Use [`FeeBumpFrame::from_envelope`] to create from a `TransactionEnvelope`. This
/// will return an error if the envelope is not a fee bump transaction.
///
/// # Example
///
/// ```ignore
/// let frame = FeeBumpFrame::from_envelope(envelope, &network_id)?;
///
/// // Access fee bump specific data
/// let fee_source = frame.fee_source();
/// let outer_fee = frame.outer_fee();
/// let inner_fee = frame.inner_fee();
///
/// // Get inner transaction hash
/// let inner_hash = frame.inner_transaction_hash()?;
/// ```
#[derive(Debug, Clone)]
pub struct FeeBumpFrame {
    /// The underlying transaction frame.
    frame: TransactionFrame,
    /// The inner transaction frame wrapped by the fee bump.
    inner_frame: TransactionFrame,
    /// Network ID for hash computation.
    network_id: NetworkId,
    /// Cached inner transaction hash.
    inner_hash: Option<Hash256>,
}

impl FeeBumpFrame {
    /// Create a FeeBumpFrame from a TransactionEnvelope.
    ///
    /// Returns an error if the envelope is not a fee bump transaction.
    pub fn from_envelope(
        envelope: TransactionEnvelope,
        network_id: &NetworkId,
    ) -> std::result::Result<Self, FeeBumpError> {
        if !matches!(envelope, TransactionEnvelope::TxFeeBump(_)) {
            return Err(FeeBumpError::NotFeeBump);
        }

        let inner_envelope = match &envelope {
            TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
                FeeBumpTransactionInnerTx::Tx(inner) => inner.clone(),
            },
            _ => unreachable!("FeeBumpFrame always contains a fee bump transaction"),
        };
        let inner_frame = TransactionFrame::from_owned_with_network(
            TransactionEnvelope::Tx(inner_envelope),
            *network_id,
        );

        Ok(Self {
            frame: TransactionFrame::from_owned_with_network(envelope, *network_id),
            inner_frame,
            network_id: *network_id,
            inner_hash: None,
        })
    }

    /// Create a FeeBumpFrame from an existing TransactionFrame.
    ///
    /// Returns an error if the frame does not contain a fee bump transaction.
    pub fn from_frame(
        frame: TransactionFrame,
        network_id: &NetworkId,
    ) -> std::result::Result<Self, FeeBumpError> {
        if !frame.is_fee_bump() {
            return Err(FeeBumpError::NotFeeBump);
        }

        let inner_envelope = match frame.envelope() {
            TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
                FeeBumpTransactionInnerTx::Tx(inner) => inner.clone(),
            },
            _ => unreachable!("FeeBumpFrame always contains a fee bump transaction"),
        };
        let inner_frame = TransactionFrame::from_owned_with_network(
            TransactionEnvelope::Tx(inner_envelope),
            *network_id,
        );

        Ok(Self {
            frame,
            inner_frame,
            network_id: *network_id,
            inner_hash: None,
        })
    }

    /// Get the underlying TransactionFrame.
    pub fn frame(&self) -> &TransactionFrame {
        &self.frame
    }

    /// Consume and return the underlying TransactionFrame.
    pub fn into_frame(self) -> TransactionFrame {
        self.frame
    }

    /// Get the fee bump transaction data.
    fn fee_bump_tx(&self) -> &FeeBumpTransaction {
        match self.frame.envelope() {
            TransactionEnvelope::TxFeeBump(env) => &env.tx,
            _ => unreachable!("FeeBumpFrame always contains a fee bump transaction"),
        }
    }

    /// Get the fee bump envelope.
    fn fee_bump_envelope(&self) -> &FeeBumpTransactionEnvelope {
        match self.frame.envelope() {
            TransactionEnvelope::TxFeeBump(env) => env,
            _ => unreachable!("FeeBumpFrame always contains a fee bump transaction"),
        }
    }

    /// Get the inner V1 envelope.
    fn inner_envelope(&self) -> &TransactionV1Envelope {
        match &self.fee_bump_tx().inner_tx {
            FeeBumpTransactionInnerTx::Tx(inner) => inner,
        }
    }

    /// Get the fee source account (outer source that pays the fee).
    pub fn fee_source(&self) -> &MuxedAccount {
        &self.fee_bump_tx().fee_source
    }

    /// Get the fee source account ID.
    pub fn fee_source_account_id(&self) -> AccountId {
        muxed_to_account_id(self.fee_source())
    }

    /// Get the inner transaction source account.
    pub fn inner_source(&self) -> &MuxedAccount {
        &self.inner_envelope().tx.source_account
    }

    /// Get the inner source account ID.
    pub fn inner_source_account_id(&self) -> AccountId {
        muxed_to_account_id(self.inner_source())
    }

    /// Get the outer fee (total fee to be charged).
    pub fn outer_fee(&self) -> i64 {
        self.fee_bump_tx().fee
    }

    /// Get the inner transaction fee.
    pub fn inner_fee(&self) -> u32 {
        self.inner_envelope().tx.fee
    }

    /// Get the number of operations in the inner transaction.
    pub fn operation_count(&self) -> usize {
        self.inner_envelope().tx.operations.len()
    }

    /// Get the outer signatures (fee bump signatures).
    pub fn outer_signatures(&self) -> &[DecoratedSignature] {
        self.fee_bump_envelope().signatures.as_slice()
    }

    /// Get the inner transaction signatures.
    pub fn inner_signatures(&self) -> &[DecoratedSignature] {
        self.inner_envelope().signatures.as_slice()
    }

    /// Compute the fee bump transaction hash (outer hash).
    pub fn hash(&self) -> Result<Hash256> {
        self.frame.hash(&self.network_id)
    }

    /// Compute and cache the inner transaction hash.
    ///
    /// This is the hash of the inner V1 transaction, used for:
    /// - Result reporting (InnerTransactionResultPair)
    /// - Inner signature verification
    pub fn inner_transaction_hash(&mut self) -> Result<Hash256> {
        if let Some(hash) = self.inner_hash {
            return Ok(hash);
        }

        let hash = self.inner_frame.hash(&self.network_id)?;

        self.inner_hash = Some(hash);
        Ok(hash)
    }

    /// Get the cached inner hash if available.
    pub fn cached_inner_hash(&self) -> Option<Hash256> {
        self.inner_hash
    }

    /// Check if the fee source is the same as the inner source.
    pub fn fee_source_is_inner_source(&self) -> bool {
        self.fee_source_account_id() == self.inner_source_account_id()
    }

    /// Get the sequence number (from inner transaction).
    pub fn sequence_number(&self) -> i64 {
        self.inner_envelope().tx.seq_num.0
    }

    /// Check if the inner transaction is a Soroban transaction.
    pub fn is_soroban(&self) -> bool {
        self.inner_frame.is_soroban()
    }

    /// Get the declared Soroban resource fee.
    pub fn declared_soroban_resource_fee(&self) -> crate::fees::ResourceFee {
        self.inner_frame.declared_soroban_resource_fee()
    }

    /// Get a reference to the inner transaction frame.
    ///
    /// This provides access to the inner V1 transaction wrapped by this fee bump.
    pub fn inner_frame(&self) -> &TransactionFrame {
        &self.inner_frame
    }

    /// Get the refundable fee for Soroban transactions.
    ///
    /// Delegates to the inner frame's refundable fee calculation.
    pub fn refundable_fee(&self) -> Option<i64> {
        self.inner_frame.refundable_fee()
    }
}

/// Validate fee bump specific rules.
///
/// This performs fee bump-specific validation including:
/// - Outer fee must be >= inner fee (with base fee multiplier)
/// - Inner transaction must be valid V1 transaction
/// - Inner signatures must be cryptographically valid
///
/// # Parameters
///
/// - `frame`: The fee bump frame to validate
/// - `context`: Ledger context providing base fee and protocol version
///
/// # Returns
///
/// `Ok(())` if validation passes, or `Err(FeeBumpError)` with details.
pub fn validate_fee_bump(
    frame: &mut FeeBumpFrame,
    context: &LedgerContext,
) -> std::result::Result<(), FeeBumpError> {
    // Validate inclusion fee against min fee and ensure the fee bump is actually bumping.
    // This mirrors stellar-core FeeBumpTransactionFrame::commonValidPreSeqNum logic.
    if !frame
        .frame()
        .has_sufficient_inclusion_fee(context.base_fee as i64)
    {
        return Err(FeeBumpError::InsufficientOuterFee {
            outer_fee: frame.outer_fee(),
            required_min: frame
                .frame()
                .min_inclusion_fee(context.base_fee as i64)
                .as_i64(),
        });
    }

    // These locals are needed for the fee-bump rate cross-multiplication below.
    let outer_min_inclusion_fee = frame.frame().min_inclusion_fee(context.base_fee as i64);
    let outer_inclusion_fee = frame.frame().inclusion_fee();

    let inner_inclusion_fee = frame.inner_frame().inclusion_fee();
    if inner_inclusion_fee.as_i64() >= 0 {
        let inner_min_inclusion_fee = frame
            .inner_frame()
            .min_inclusion_fee(context.base_fee as i64);
        let v1 = outer_inclusion_fee.as_i64() as i128 * inner_min_inclusion_fee.as_i64() as i128;
        let v2 = inner_inclusion_fee.as_i64() as i128 * outer_min_inclusion_fee.as_i64() as i128;
        if v1 < v2 {
            let required_outer = ((v2 + inner_min_inclusion_fee.as_i64() as i128 - 1)
                / inner_min_inclusion_fee.as_i64() as i128) as i64;
            return Err(FeeBumpError::InsufficientOuterFee {
                outer_fee: frame.outer_fee(),
                required_min: std::cmp::max(outer_min_inclusion_fee.as_i64(), required_outer),
            });
        }
    } else {
        let allow_negative_inner = frame.inner_frame().is_soroban();
        if !allow_negative_inner {
            return Err(FeeBumpError::InvalidInnerTxType);
        }
    }

    // Validate Soroban resource_fee upper bound (XDRProvidesValidFee).
    // stellar-core: TransactionFrame.cpp:1773
    if frame.inner_frame().is_soroban() {
        let resource_fee = frame.inner_frame().declared_soroban_resource_fee().as_i64();
        if resource_fee < 0 || resource_fee > crate::validation::MAX_RESOURCE_FEE {
            return Err(FeeBumpError::InvalidInnerTxType);
        }
    }

    // Validate inner transaction operation count
    if frame.operation_count() == 0 {
        return Err(FeeBumpError::TooManyOperations(0));
    }
    if frame.operation_count() > crate::MAX_OPS_PER_TX {
        return Err(FeeBumpError::TooManyOperations(frame.operation_count()));
    }

    // Compute inner hash (needed for inner signature verification)
    let inner_hash = frame
        .inner_transaction_hash()
        .map_err(|e| FeeBumpError::HashError(e.to_string()))?;

    // Note: Do NOT validate inner signature lengths here. Signatures can be
    // variable-length (Hash-X preimages are 1-64 bytes, pre-auth TX sigs are
    // 0 bytes). stellar-core's commonValidPreSeqNum has no inner signature
    // length check — format validation is handled in signature_checker.rs
    // during application. See #1122 / AUDIT-047.

    let _ = inner_hash; // Used for caching
    Ok(())
}

/// Verify inner transaction signatures against a hash.
///
/// This performs cryptographic verification of inner signatures. It's separate
/// from `validate_fee_bump` because it requires the full public keys from account
/// data, not just the signature hints.
// SECURITY: actual transaction auth uses has_sufficient_signer_weight() with threshold accumulation, not this helper
pub fn verify_inner_signatures(
    inner_hash: &Hash256,
    signatures: &[DecoratedSignature],
    public_keys: &[henyey_crypto::PublicKey],
) -> bool {
    use crate::validation::verify_signature_with_key;

    // An empty signature list must not pass verification.
    // Without this check, the for-loop below returns true by vacuous truth.
    // stellar-core's SignatureChecker::checkSignature returns false when no
    // signatures match any signer, regardless of neededWeight.
    if signatures.is_empty() {
        return false;
    }

    for sig in signatures {
        // Find matching public key by hint
        let hint = &sig.hint.0;
        let matching_key = public_keys.iter().find(|pk| {
            let key_bytes = pk.as_bytes();
            [key_bytes[28], key_bytes[29], key_bytes[30], key_bytes[31]] == *hint
        });

        if let Some(pk) = matching_key {
            if verify_signature_with_key(inner_hash, sig, pk) {
                continue;
            }
        }
        return false;
    }

    true
}

/// Mutable result for fee bump transactions.
///
/// This tracks both the outer fee bump result and the inner transaction result,
/// providing proper result wrapping as required by the Stellar protocol.
///
/// # Inner Result Tracking
///
/// Fee bump results wrap the inner transaction result in an `InnerTransactionResultPair`,
/// which includes:
/// - The inner transaction hash
/// - The inner transaction result with its fee charged
///
/// # Fee Tracking
///
/// For Soroban transactions:
/// - Pre-protocol 25: Inner fee is charged, outer only pays the difference
/// - Protocol 25+: Inner fee is 0, outer fee covers everything
///
/// # Example
///
/// ```ignore
/// let mut result = FeeBumpMutableTransactionResult::new(
///     outer_fee,
///     inner_hash,
///     inner_fee_charged,
///     op_count,
/// );
///
/// // Set inner operation results
/// result.set_inner_operation_result(0, op_result);
///
/// // On error
/// result.set_inner_error(TxResultCode::TxFailed);
///
/// // Finalize
/// let xdr = result.into_xdr();
/// ```
#[derive(Debug, Clone)]
pub struct FeeBumpMutableTransactionResult {
    /// Outer fee charged (total).
    outer_fee_charged: i64,
    /// Inner transaction hash.
    inner_tx_hash: Hash256,
    /// Inner fee charged.
    inner_fee_charged: i64,
    /// Inner transaction success status.
    inner_success: bool,
    /// Inner operation results.
    inner_op_results: Vec<OperationResult>,
    /// Inner transaction result code (when failed).
    inner_result_code: Option<InnerTransactionResultResult>,
    /// Refundable fee tracker for Soroban.
    refundable_fee_tracker: Option<RefundableFeeTracker>,
}

impl FeeBumpMutableTransactionResult {
    /// Create a new fee bump result.
    ///
    /// # Parameters
    ///
    /// - `outer_fee_charged`: Total fee charged from fee source
    /// - `inner_tx_hash`: Hash of the inner transaction
    /// - `inner_fee_charged`: Fee attributed to inner transaction (may be 0 in P25+)
    /// - `op_count`: Number of operations in inner transaction
    pub fn new(
        outer_fee_charged: i64,
        inner_tx_hash: Hash256,
        inner_fee_charged: i64,
        op_count: usize,
    ) -> Self {
        Self {
            outer_fee_charged,
            inner_tx_hash,
            inner_fee_charged,
            inner_success: true,
            inner_op_results: vec![
                OperationResult::OpInner(
                    stellar_xdr::curr::OperationResultTr::Payment(
                        stellar_xdr::curr::PaymentResult::Success,
                    )
                );
                op_count
            ],
            inner_result_code: None,
            refundable_fee_tracker: None,
        }
    }

    /// Create an error result.
    pub fn create_error(
        outer_fee_charged: i64,
        inner_tx_hash: Hash256,
        inner_fee_charged: i64,
        error_code: InnerTransactionResultResult,
    ) -> Self {
        Self {
            outer_fee_charged,
            inner_tx_hash,
            inner_fee_charged,
            inner_success: false,
            inner_op_results: Vec::new(),
            inner_result_code: Some(error_code),
            refundable_fee_tracker: None,
        }
    }

    /// Set the inner transaction result code for errors.
    pub fn set_inner_error(&mut self, code: InnerTransactionResultResult) {
        self.inner_success = false;
        self.inner_result_code = Some(code);

        // Reset refundable fees on error
        if let Some(ref mut tracker) = self.refundable_fee_tracker {
            tracker.reset_consumed_fee();
        }
    }

    /// Set an inner operation result.
    pub fn set_inner_operation_result(&mut self, index: usize, result: OperationResult) {
        if index < self.inner_op_results.len() {
            self.inner_op_results[index] = result;
        }
    }

    /// Get the inner operation results.
    pub fn inner_operation_results(&self) -> &[OperationResult] {
        &self.inner_op_results
    }

    /// Initialize refundable fee tracker for Soroban.
    pub fn initialize_refundable_fee_tracker(&mut self, max_refundable_fee: i64) {
        self.refundable_fee_tracker = Some(RefundableFeeTracker::new(max_refundable_fee));
    }

    /// Get mutable reference to refundable fee tracker.
    pub fn refundable_fee_tracker_mut(&mut self) -> Option<&mut RefundableFeeTracker> {
        self.refundable_fee_tracker.as_mut()
    }

    /// Get reference to refundable fee tracker.
    pub fn refundable_fee_tracker(&self) -> Option<&RefundableFeeTracker> {
        self.refundable_fee_tracker.as_ref()
    }

    /// Finalize fee refund.
    ///
    /// Matches stellar-core `FeeBumpMutableTransactionResult::finalizeFeeRefund()`
    /// (MutableTransactionResult.cpp:420-443):
    /// - Protocol >= 25: Refund applied to outer fee only (inner fee is always 0 in P25+).
    /// - Protocol 21-24: Refund applied to both outer and inner fee.
    ///
    /// Note: henyey only supports protocol 24+. The else branch handles P24
    /// specifically; protocols < 21 would not apply a refund in stellar-core,
    /// but that case never arises here.
    pub fn finalize_fee_refund(&mut self, protocol_version: u32) {
        if let Some(ref tracker) = self.refundable_fee_tracker {
            let refund = tracker.get_fee_refund();
            self.outer_fee_charged -= refund;
            if fee_bump_refund_applies_to_inner(protocol_version) {
                self.inner_fee_charged -= refund;
            }
        }
    }

    /// Check if the inner transaction succeeded.
    pub fn is_success(&self) -> bool {
        self.inner_success
    }

    /// Get the outer fee charged.
    pub fn outer_fee_charged(&self) -> i64 {
        self.outer_fee_charged
    }

    /// Get the inner fee charged.
    pub fn inner_fee_charged(&self) -> i64 {
        self.inner_fee_charged
    }

    /// Set the outer fee charged.
    pub fn set_outer_fee_charged(&mut self, fee: i64) {
        self.outer_fee_charged = fee;
    }

    /// Set the inner fee charged.
    pub fn set_inner_fee_charged(&mut self, fee: i64) {
        self.inner_fee_charged = fee;
    }

    /// Get the result code.
    pub fn result_code(&self) -> TxResultCode {
        if self.inner_success {
            TxResultCode::TxFeeBumpInnerSuccess
        } else {
            TxResultCode::TxFeeBumpInnerFailed
        }
    }

    /// Convert to wrapper for inspection.
    pub fn to_wrapper(&self) -> TxResultWrapper {
        TxResultWrapper::from_xdr(self.to_xdr())
    }

    /// Build the XDR TransactionResult.
    pub fn to_xdr(&self) -> TransactionResult {
        let inner_result = if self.inner_success {
            InnerTransactionResultResult::TxSuccess(
                self.inner_op_results.clone().try_into().unwrap_or_default(),
            )
        } else {
            self.inner_result_code
                .clone()
                .unwrap_or(InnerTransactionResultResult::TxFailed(
                    self.inner_op_results.clone().try_into().unwrap_or_default(),
                ))
        };

        let inner_result_pair = InnerTransactionResultPair {
            transaction_hash: Hash(self.inner_tx_hash.0),
            result: InnerTransactionResult {
                fee_charged: self.inner_fee_charged,
                result: inner_result,
                ext: InnerTransactionResultExt::V0,
            },
        };

        let result = if self.inner_success {
            TransactionResultResult::TxFeeBumpInnerSuccess(inner_result_pair)
        } else {
            TransactionResultResult::TxFeeBumpInnerFailed(inner_result_pair)
        };

        TransactionResult {
            fee_charged: self.outer_fee_charged,
            result,
            ext: TransactionResultExt::V0,
        }
    }

    /// Consume and return the XDR result.
    pub fn into_xdr(self) -> TransactionResult {
        self.to_xdr()
    }
}

/// Determine whether a fee-bump refund should be applied to the inner fee_charged field.
///
/// - Protocol >= 25: refund is NOT applied to inner (inner fee is always 0 in P25+)
/// - Protocol < 25 (henyey supports 24+): refund IS applied to inner
///   (stellar-core bug replicated for parity; see MutableTransactionResult.cpp:421-443)
///
/// Note: henyey only supports protocol 24+. The `true` branch handles P24
/// specifically; protocols < 21 would not apply a refund in stellar-core,
/// but that case never arises here.
///
/// References:
/// - stellar-core `MutableTransactionResult.cpp:421-443`
/// - stellar-core `FeeBumpTransactionFrame.cpp:696-700`
pub fn fee_bump_refund_applies_to_inner(protocol_version: u32) -> bool {
    !protocol_version_starts_from(protocol_version, ProtocolVersion::V25)
}

/// Calculate the inner fee charged for a fee bump transaction.
///
/// # Protocol Behavior
///
/// - Protocol < 25: Inner fee is charged to inner source account
/// - Protocol >= 25: Inner fee is 0, outer pays everything
///
/// This matches stellar-core `FeeBumpTransactionFrame::getInnerFullFee`.
pub fn calculate_inner_fee_charged(inner_declared_fee: u32, protocol_version: u32) -> i64 {
    if fee_bump_refund_applies_to_inner(protocol_version) {
        inner_declared_fee as i64
    } else {
        0
    }
}

/// Create a fee bump result from an inner transaction result.
///
/// This is useful when you have already executed the inner transaction and
/// need to wrap it in a fee bump result.
pub fn wrap_inner_result_in_fee_bump(
    inner_hash: Hash256,
    inner_result: &TransactionResult,
    outer_fee_charged: i64,
) -> TransactionResult {
    let inner_success = matches!(inner_result.result, TransactionResultResult::TxSuccess(_));

    let inner_result_result = match &inner_result.result {
        TransactionResultResult::TxSuccess(ops) => {
            InnerTransactionResultResult::TxSuccess(ops.clone())
        }
        TransactionResultResult::TxFailed(ops) => {
            InnerTransactionResultResult::TxFailed(ops.clone())
        }
        TransactionResultResult::TxTooEarly => InnerTransactionResultResult::TxTooEarly,
        TransactionResultResult::TxTooLate => InnerTransactionResultResult::TxTooLate,
        TransactionResultResult::TxMissingOperation => {
            InnerTransactionResultResult::TxMissingOperation
        }
        TransactionResultResult::TxBadSeq => InnerTransactionResultResult::TxBadSeq,
        TransactionResultResult::TxBadAuth => InnerTransactionResultResult::TxBadAuth,
        TransactionResultResult::TxInsufficientBalance => {
            InnerTransactionResultResult::TxInsufficientBalance
        }
        TransactionResultResult::TxNoAccount => InnerTransactionResultResult::TxNoAccount,
        TransactionResultResult::TxInsufficientFee => {
            InnerTransactionResultResult::TxInsufficientFee
        }
        TransactionResultResult::TxBadAuthExtra => InnerTransactionResultResult::TxBadAuthExtra,
        TransactionResultResult::TxInternalError => InnerTransactionResultResult::TxInternalError,
        TransactionResultResult::TxNotSupported => InnerTransactionResultResult::TxNotSupported,
        TransactionResultResult::TxBadSponsorship => InnerTransactionResultResult::TxBadSponsorship,
        TransactionResultResult::TxBadMinSeqAgeOrGap => {
            InnerTransactionResultResult::TxBadMinSeqAgeOrGap
        }
        TransactionResultResult::TxMalformed => InnerTransactionResultResult::TxMalformed,
        TransactionResultResult::TxSorobanInvalid => InnerTransactionResultResult::TxSorobanInvalid,
        TransactionResultResult::TxFrozenKeyAccessed => {
            InnerTransactionResultResult::TxFrozenKeyAccessed
        }
        // Fee bump results shouldn't be nested
        TransactionResultResult::TxFeeBumpInnerSuccess(pair) => pair.result.result.clone(),
        TransactionResultResult::TxFeeBumpInnerFailed(pair) => pair.result.result.clone(),
    };

    let inner_result_pair = InnerTransactionResultPair {
        transaction_hash: Hash(inner_hash.0),
        result: InnerTransactionResult {
            fee_charged: inner_result.fee_charged,
            result: inner_result_result,
            ext: InnerTransactionResultExt::V0,
        },
    };

    let result = if inner_success {
        TransactionResultResult::TxFeeBumpInnerSuccess(inner_result_pair)
    } else {
        TransactionResultResult::TxFeeBumpInnerFailed(inner_result_pair)
    };

    TransactionResult {
        fee_charged: outer_fee_charged,
        result,
        ext: TransactionResultExt::V0,
    }
}

/// Extract the inner transaction hash from a fee bump result.
///
/// Returns `None` if the result is not a fee bump result.
pub fn extract_inner_hash_from_result(result: &TransactionResult) -> Option<Hash256> {
    match &result.result {
        TransactionResultResult::TxFeeBumpInnerSuccess(pair)
        | TransactionResultResult::TxFeeBumpInnerFailed(pair) => {
            Some(Hash256(pair.transaction_hash.0))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_common::NetworkId;
    use stellar_xdr::curr::{
        Asset, FeeBumpTransactionExt, Memo, Operation, OperationBody, PaymentOp, Preconditions,
        SequenceNumber, Signature, SignatureHint, Transaction, TransactionExt, Uint256, VecM,
    };

    fn create_inner_v1_envelope(fee: u32, op_count: usize) -> TransactionV1Envelope {
        let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([2u8; 32]));

        let ops: Vec<Operation> = (0..op_count)
            .map(|_| Operation {
                source_account: None,
                body: OperationBody::Payment(PaymentOp {
                    destination: dest.clone(),
                    asset: Asset::Native,
                    amount: 1000,
                }),
            })
            .collect();

        let tx = Transaction {
            source_account: source,
            fee,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: ops.try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        }
    }

    fn create_fee_bump_envelope(
        inner_fee: u32,
        outer_fee: i64,
        op_count: usize,
    ) -> TransactionEnvelope {
        let inner = create_inner_v1_envelope(inner_fee, op_count);
        let fee_source = MuxedAccount::Ed25519(Uint256([3u8; 32]));

        let fee_bump = FeeBumpTransaction {
            fee_source,
            fee: outer_fee,
            inner_tx: FeeBumpTransactionInnerTx::Tx(inner),
            ext: FeeBumpTransactionExt::V0,
        };

        TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: fee_bump,
            signatures: VecM::default(),
        })
    }

    #[test]
    fn test_fee_bump_frame_from_envelope() {
        let envelope = create_fee_bump_envelope(100, 200, 1);
        let network_id = NetworkId::testnet();

        let frame = FeeBumpFrame::from_envelope(envelope, &network_id);
        assert!(frame.is_ok());

        let frame = frame.unwrap();
        assert_eq!(frame.outer_fee(), 200);
        assert_eq!(frame.inner_fee(), 100);
        assert_eq!(frame.operation_count(), 1);
    }

    #[test]
    fn test_fee_bump_frame_not_fee_bump() {
        let source = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([2u8; 32]));

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::Payment(PaymentOp {
                    destination: dest,
                    asset: Asset::Native,
                    amount: 1000,
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });

        let network_id = NetworkId::testnet();
        let result = FeeBumpFrame::from_envelope(envelope, &network_id);

        assert!(matches!(result, Err(FeeBumpError::NotFeeBump)));
    }

    #[test]
    fn test_fee_bump_frame_inner_hash() {
        let envelope = create_fee_bump_envelope(100, 200, 1);
        let network_id = NetworkId::testnet();

        let mut frame = FeeBumpFrame::from_envelope(envelope, &network_id).unwrap();

        // First call computes and caches
        let hash1 = frame.inner_transaction_hash().unwrap();
        assert!(frame.cached_inner_hash().is_some());

        // Second call returns cached
        let hash2 = frame.inner_transaction_hash().unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_validate_fee_bump_sufficient_fee() {
        let envelope = create_fee_bump_envelope(100, 200, 1);
        let network_id = NetworkId::testnet();

        let mut frame = FeeBumpFrame::from_envelope(envelope, &network_id).unwrap();
        let context = LedgerContext::testnet(1, 1000);

        // Outer fee 200 >= inner fee 100 and >= base_fee * (1 + 1) = 200
        let result = validate_fee_bump(&mut frame, &context);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_fee_bump_insufficient_fee() {
        // Outer fee 150 < inner fee 100 * 2 operations = 200 min
        let envelope = create_fee_bump_envelope(100, 150, 2);
        let network_id = NetworkId::testnet();

        let mut frame = FeeBumpFrame::from_envelope(envelope, &network_id).unwrap();
        let context = LedgerContext::testnet(1, 1000);

        let result = validate_fee_bump(&mut frame, &context);
        assert!(matches!(
            result,
            Err(FeeBumpError::InsufficientOuterFee { .. })
        ));
    }

    #[test]
    fn test_validate_fee_bump_outer_must_exceed_inner() {
        // Outer fee 90 < inner fee 100
        let envelope = create_fee_bump_envelope(100, 90, 1);
        let network_id = NetworkId::testnet();

        let mut frame = FeeBumpFrame::from_envelope(envelope, &network_id).unwrap();
        let context = LedgerContext::testnet(1, 1000);

        let result = validate_fee_bump(&mut frame, &context);
        assert!(matches!(
            result,
            Err(FeeBumpError::InsufficientOuterFee { .. })
        ));
    }

    #[test]
    fn test_fee_bump_mutable_result_new() {
        let inner_hash = Hash256([1u8; 32]);
        let result = FeeBumpMutableTransactionResult::new(1000, inner_hash, 500, 2);

        assert!(result.is_success());
        assert_eq!(result.outer_fee_charged(), 1000);
        assert_eq!(result.inner_fee_charged(), 500);
        assert_eq!(result.inner_operation_results().len(), 2);
        assert_eq!(result.result_code(), TxResultCode::TxFeeBumpInnerSuccess);
    }

    #[test]
    fn test_fee_bump_mutable_result_set_error() {
        let inner_hash = Hash256([1u8; 32]);
        let mut result = FeeBumpMutableTransactionResult::new(1000, inner_hash, 500, 2);

        result.set_inner_error(InnerTransactionResultResult::TxBadSeq);

        assert!(!result.is_success());
        assert_eq!(result.result_code(), TxResultCode::TxFeeBumpInnerFailed);
    }

    #[test]
    fn test_fee_bump_mutable_result_to_xdr() {
        let inner_hash = Hash256([1u8; 32]);
        let result = FeeBumpMutableTransactionResult::new(1000, inner_hash, 500, 1);
        let xdr = result.into_xdr();

        assert_eq!(xdr.fee_charged, 1000);
        assert!(matches!(
            xdr.result,
            TransactionResultResult::TxFeeBumpInnerSuccess(_)
        ));

        if let TransactionResultResult::TxFeeBumpInnerSuccess(pair) = &xdr.result {
            assert_eq!(pair.transaction_hash.0, inner_hash.0);
            assert_eq!(pair.result.fee_charged, 500);
        }
    }

    #[test]
    fn test_fee_bump_mutable_result_refundable_fee() {
        let inner_hash = Hash256([1u8; 32]);
        let mut result = FeeBumpMutableTransactionResult::new(1000, inner_hash, 0, 1);

        result.initialize_refundable_fee_tracker(400);

        if let Some(tracker) = result.refundable_fee_tracker_mut() {
            tracker.consume_rent_fee(100).unwrap();
        }

        // Protocol 25+: refund from outer
        result.finalize_fee_refund(25);
        assert_eq!(result.outer_fee_charged(), 700); // 1000 - 300 refund
    }

    #[test]
    fn test_fee_bump_mutable_result_refundable_fee_pre_p25() {
        let inner_hash = Hash256([1u8; 32]);
        let mut result = FeeBumpMutableTransactionResult::new(1000, inner_hash, 500, 1);

        result.initialize_refundable_fee_tracker(400);

        if let Some(tracker) = result.refundable_fee_tracker_mut() {
            tracker.consume_rent_fee(100).unwrap();
        }

        // Protocol 24: stellar-core deducts refund from both outer and inner
        // (MutableTransactionResult.cpp:429-441)
        result.finalize_fee_refund(24);
        assert_eq!(result.inner_fee_charged(), 200); // 500 - 300 refund
        assert_eq!(result.outer_fee_charged(), 700); // 1000 - 300 refund

        // Verify serialized XDR reflects the correct values
        let xdr = result.to_xdr();
        assert_eq!(xdr.fee_charged, 700); // outer fee in XDR
        match &xdr.result {
            TransactionResultResult::TxFeeBumpInnerSuccess(pair) => {
                assert_eq!(pair.result.fee_charged, 200); // inner fee in XDR
            }
            _ => panic!("expected TxFeeBumpInnerSuccess"),
        }
    }

    #[test]
    fn test_calculate_inner_fee_charged() {
        // Protocol 24: inner fee is charged
        assert_eq!(calculate_inner_fee_charged(500, 24), 500);

        // Protocol 25+: inner fee is 0
        assert_eq!(calculate_inner_fee_charged(500, 25), 0);
        assert_eq!(calculate_inner_fee_charged(500, 26), 0);
    }

    #[test]
    fn test_fee_bump_refund_applies_to_inner() {
        // Protocol 24: refund applies to inner (stellar-core bug replicated for parity)
        assert!(fee_bump_refund_applies_to_inner(24));

        // Protocol 25+: refund does NOT apply to inner
        assert!(!fee_bump_refund_applies_to_inner(25));
        assert!(!fee_bump_refund_applies_to_inner(26));
    }

    #[test]
    fn test_wrap_inner_result_in_fee_bump() {
        let inner_hash = Hash256([2u8; 32]);
        let inner_result = TransactionResult {
            fee_charged: 100,
            result: TransactionResultResult::TxSuccess(VecM::default()),
            ext: TransactionResultExt::V0,
        };

        let wrapped = wrap_inner_result_in_fee_bump(inner_hash, &inner_result, 500);

        assert_eq!(wrapped.fee_charged, 500);
        assert!(matches!(
            wrapped.result,
            TransactionResultResult::TxFeeBumpInnerSuccess(_)
        ));

        let extracted_hash = extract_inner_hash_from_result(&wrapped);
        assert_eq!(extracted_hash, Some(inner_hash));
    }

    #[test]
    fn test_wrap_inner_result_failed() {
        let inner_hash = Hash256([3u8; 32]);
        let inner_result = TransactionResult {
            fee_charged: 100,
            result: TransactionResultResult::TxBadSeq,
            ext: TransactionResultExt::V0,
        };

        let wrapped = wrap_inner_result_in_fee_bump(inner_hash, &inner_result, 500);

        assert!(matches!(
            wrapped.result,
            TransactionResultResult::TxFeeBumpInnerFailed(_)
        ));

        if let TransactionResultResult::TxFeeBumpInnerFailed(pair) = &wrapped.result {
            assert!(matches!(
                pair.result.result,
                InnerTransactionResultResult::TxBadSeq
            ));
        }
    }

    #[test]
    fn test_extract_inner_hash_non_fee_bump() {
        let result = TransactionResult {
            fee_charged: 100,
            result: TransactionResultResult::TxSuccess(VecM::default()),
            ext: TransactionResultExt::V0,
        };

        assert!(extract_inner_hash_from_result(&result).is_none());
    }

    #[test]
    fn test_fee_bump_frame_fee_source_is_inner_source() {
        // Different sources
        let envelope = create_fee_bump_envelope(100, 200, 1);
        let network_id = NetworkId::testnet();
        let frame = FeeBumpFrame::from_envelope(envelope, &network_id).unwrap();

        assert!(!frame.fee_source_is_inner_source());
    }

    #[test]
    fn test_fee_bump_frame_same_source() {
        let inner = create_inner_v1_envelope(100, 1);
        // Use same source as inner for fee bump
        let fee_source = inner.tx.source_account.clone();

        let fee_bump = FeeBumpTransaction {
            fee_source,
            fee: 200,
            inner_tx: FeeBumpTransactionInnerTx::Tx(inner),
            ext: FeeBumpTransactionExt::V0,
        };

        let envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: fee_bump,
            signatures: VecM::default(),
        });

        let network_id = NetworkId::testnet();
        let frame = FeeBumpFrame::from_envelope(envelope, &network_id).unwrap();

        assert!(frame.fee_source_is_inner_source());
    }

    #[test]
    fn test_fee_bump_error_display() {
        let err = FeeBumpError::InsufficientOuterFee {
            outer_fee: 100,
            required_min: 200,
        };
        assert!(err.to_string().contains("100"));
        assert!(err.to_string().contains("200"));

        let err = FeeBumpError::TooManyOperations(150);
        assert!(err.to_string().contains("150"));

        let err = FeeBumpError::NotFeeBump;
        assert!(err.to_string().contains("not a fee bump"));
    }

    #[test]
    fn test_fee_bump_mutable_result_error_resets_tracker() {
        let inner_hash = Hash256([1u8; 32]);
        let mut result = FeeBumpMutableTransactionResult::new(1000, inner_hash, 500, 1);

        result.initialize_refundable_fee_tracker(400);

        // Consume some fees
        if let Some(tracker) = result.refundable_fee_tracker_mut() {
            tracker.consume_rent_fee(200).unwrap();
        }
        assert_eq!(
            result.refundable_fee_tracker().unwrap().consumed_rent_fee(),
            200
        );

        // Set error resets tracker
        result.set_inner_error(InnerTransactionResultResult::TxFailed(VecM::default()));

        assert_eq!(
            result.refundable_fee_tracker().unwrap().consumed_rent_fee(),
            0
        );
        assert_eq!(
            result.refundable_fee_tracker().unwrap().get_fee_refund(),
            400
        );
    }

    /// AUDIT-C7: Empty signature list must not pass verification.
    ///
    /// An empty signatures vec causes the for-loop in verify_inner_signatures
    /// to never execute, returning true (vacuous truth). stellar-core's
    /// SignatureChecker::checkSignature returns false when no signatures
    /// match, even with neededWeight=0. Empty sigs must be rejected.
    #[test]
    fn test_audit_c7_empty_signatures_returns_false() {
        let inner_hash = Hash256([42u8; 32]);
        let signatures: &[DecoratedSignature] = &[];
        let pk = henyey_crypto::PublicKey::from_bytes(&[1u8; 32]).unwrap();
        let public_keys = &[pk];

        assert!(
            !verify_inner_signatures(&inner_hash, signatures, public_keys),
            "empty signature list must not pass verification (vacuous truth bypass)"
        );
    }

    /// #1122 / AUDIT-047: Fee bump must accept inner signatures of any length.
    ///
    /// stellar-core's commonValidPreSeqNum has no inner signature length check.
    /// Hash-X signatures can be any length (the preimage), pre-auth TX sigs are
    /// 0 bytes, and Ed25519 sigs are exactly 64 bytes. All must be accepted
    /// during fee bump validation; format checking happens in signature_checker.
    #[test]
    fn test_validate_fee_bump_accepts_variable_length_inner_signatures() {
        let network_id = NetworkId::testnet();
        let context = LedgerContext::testnet(1, 1000);

        // Helper: create fee bump envelope with given inner signatures
        let make_envelope = |sig_lengths: &[usize]| {
            let inner = create_inner_v1_envelope(100, 1);
            let sigs: Vec<DecoratedSignature> = sig_lengths
                .iter()
                .map(|&len| DecoratedSignature {
                    hint: SignatureHint([0u8; 4]),
                    signature: Signature(vec![0xAB; len].try_into().unwrap()),
                })
                .collect();
            let mut inner_env = inner;
            inner_env.signatures = sigs.try_into().unwrap();

            let fee_source = MuxedAccount::Ed25519(Uint256([3u8; 32]));
            let fee_bump = FeeBumpTransaction {
                fee_source,
                fee: 200,
                inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
                ext: FeeBumpTransactionExt::V0,
            };
            TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
                tx: fee_bump,
                signatures: VecM::default(),
            })
        };

        // Ed25519 signature (64 bytes) — should pass
        let envelope = make_envelope(&[64]);
        let mut frame = FeeBumpFrame::from_envelope(envelope, &network_id).unwrap();
        assert!(validate_fee_bump(&mut frame, &context).is_ok());

        // Hash-X preimage (32 bytes) — should pass
        let envelope = make_envelope(&[32]);
        let mut frame = FeeBumpFrame::from_envelope(envelope, &network_id).unwrap();
        assert!(
            validate_fee_bump(&mut frame, &context).is_ok(),
            "32-byte Hash-X preimage inner sig must be accepted"
        );

        // Pre-auth TX signature (0 bytes) — should pass
        let envelope = make_envelope(&[0]);
        let mut frame = FeeBumpFrame::from_envelope(envelope, &network_id).unwrap();
        assert!(
            validate_fee_bump(&mut frame, &context).is_ok(),
            "0-byte pre-auth TX inner sig must be accepted"
        );

        // Short Hash-X preimage (1 byte) — should pass
        let envelope = make_envelope(&[1]);
        let mut frame = FeeBumpFrame::from_envelope(envelope, &network_id).unwrap();
        assert!(
            validate_fee_bump(&mut frame, &context).is_ok(),
            "1-byte Hash-X preimage inner sig must be accepted"
        );

        // Mixed lengths (Ed25519 + Hash-X) — should pass
        let envelope = make_envelope(&[64, 32]);
        let mut frame = FeeBumpFrame::from_envelope(envelope, &network_id).unwrap();
        assert!(
            validate_fee_bump(&mut frame, &context).is_ok(),
            "mixed-length inner sigs must be accepted"
        );
    }
}
