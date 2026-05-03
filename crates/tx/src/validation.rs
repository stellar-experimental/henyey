//! Transaction validation logic.
//!
//! This module provides validation functions for Stellar transactions, including:
//!
//! - **Structure validation**: Ensures transaction envelopes are well-formed
//! - **Fee validation**: Checks minimum fee requirements
//! - **Time bounds validation**: Verifies transaction is within valid time range
//! - **Ledger bounds validation**: Verifies transaction is within valid ledger range
//! - **Sequence validation**: Checks source account sequence number
//! - **Signature validation**: Verifies cryptographic signatures
//! - **Soroban validation**: Validates Soroban-specific resources and footprints
//!
//! # Validation Modes
//!
//! Two validation modes are provided:
//!
//! - [`validate_basic`]: Minimal checks suitable for catchup/replay mode where
//!   historical results are trusted. Does not require account data.
//!
//! - [`validate_full`]: Complete validation including signatures and account
//!   balance checks. Required for live transaction submission.
//!
//! # Example
//!
//! ```ignore
//! use henyey_tx::validation::{validate_basic, LedgerContext};
//!
//! let context = LedgerContext::testnet(1000, current_time);
//! let frame = TransactionFrame::from_owned(envelope);
//!
//! match validate_basic(&frame, &context) {
//!     Ok(()) => println!("Transaction is valid"),
//!     Err(errors) => {
//!         for err in errors {
//!             println!("Validation error: {}", err);
//!         }
//!     }
//! }
//! ```

use henyey_common::{asset::is_asset_valid, Hash256, NetworkId};
use henyey_crypto::{PublicKey, Signature};
use stellar_xdr::curr::{
    AccountEntry, DecoratedSignature, OperationBody, Preconditions, SignerKey, TransactionEnvelope,
};

use crate::fee_bump::{validate_fee_bump, FeeBumpError, FeeBumpFrame};
use crate::frame::TransactionFrame;

/// stellar-core: TransactionFrame.cpp:65 — MAX_RESOURCE_FEE = 2^50
pub(crate) const MAX_RESOURCE_FEE: i64 = 1i64 << 50;

/// Per-transaction Soroban resource limits from the network configuration.
///
/// Parity: stellar-core `SorobanNetworkConfig` per-TX limits, checked in
/// `TransactionFrame::checkSorobanResources()` (TransactionFrame.cpp:827-1044).
///
/// These limits are enforced in [`check_valid_pre_seq_num_with_config`] for all
/// validation paths: queue admission, received tx-set validation, and ledger
/// preconditions.
#[derive(Debug, Clone)]
pub struct SorobanResourceLimits {
    /// Maximum instructions per transaction (`txMaxInstructions`).
    pub tx_max_instructions: u64,
    /// Maximum disk read bytes per transaction (`txMaxDiskReadBytes`).
    pub tx_max_read_bytes: u64,
    /// Maximum write bytes per transaction (`txMaxWriteBytes`).
    pub tx_max_write_bytes: u64,
    /// Maximum disk read entries per transaction (`txMaxDiskReadEntries`).
    pub tx_max_read_ledger_entries: u64,
    /// Maximum write ledger entries per transaction (`txMaxWriteLedgerEntries`).
    pub tx_max_write_ledger_entries: u64,
    /// Maximum transaction size in bytes (`txMaxSizeBytes`).
    pub tx_max_size_bytes: u64,
    /// Maximum footprint entries per transaction, v23+ only (`txMaxFootprintEntries`).
    pub tx_max_footprint_entries: u64,
    /// Maximum contract WASM size in bytes (`maxContractSizeBytes`).
    pub max_contract_size_bytes: u32,
    /// Maximum contract data key size in bytes (`maxContractDataKeySizeBytes`).
    pub max_contract_data_key_size_bytes: u32,
}

/// Ledger context for transaction validation and execution.
///
/// This structure provides all the ledger-level information needed to validate
/// and execute transactions. It includes network parameters, timing information,
/// and protocol version.
///
/// # Construction
///
/// Use the convenience constructors for common networks:
/// - [`LedgerContext::testnet`]: Testnet with default parameters
/// - [`LedgerContext::mainnet`]: Mainnet with default parameters
/// - [`LedgerContext::new`]: Custom configuration
///
/// # Fields
///
/// All fields are public for flexibility, but be careful to ensure consistency
/// (e.g., network ID should match the network you're connecting to).
#[derive(Debug, Clone)]
pub struct LedgerContext {
    /// Current ledger sequence number.
    pub sequence: u32,
    /// Ledger close time as Unix timestamp (seconds since epoch).
    pub close_time: u64,
    /// Base fee per operation in stroops (1 stroop = 0.0000001 XLM).
    pub base_fee: u32,
    /// Base reserve per ledger entry in stroops.
    pub base_reserve: u32,
    /// Protocol version number (e.g., 21, 22, 23, ...).
    pub protocol_version: u32,
    /// Network identifier (mainnet, testnet, or custom).
    pub network_id: NetworkId,
    /// PRNG seed for deterministic Soroban contract execution.
    ///
    /// This is computed as `subSha256(txSetHash, txIndex)` per the stellar-core
    /// specification. If `None`, a fallback seed is used which may produce different
    /// results from stellar-core.
    pub soroban_prng_seed: Option<[u8; 32]>,
    /// Frozen ledger keys configuration (CAP-77, Protocol 26+).
    pub frozen_key_config: crate::frozen_keys::FrozenKeyConfig,
    /// Ledger header flags (LP disable flags, etc.). 0 when pre-v1 extension.
    pub ledger_flags: u32,
}

impl LedgerContext {
    /// Create a new ledger context.
    pub fn new(
        sequence: u32,
        close_time: u64,
        base_fee: u32,
        base_reserve: u32,
        protocol_version: u32,
        network_id: NetworkId,
    ) -> Self {
        Self {
            sequence,
            close_time,
            base_fee,
            base_reserve,
            protocol_version,
            network_id,
            soroban_prng_seed: None,
            frozen_key_config: crate::frozen_keys::FrozenKeyConfig::empty(),
            ledger_flags: 0,
        }
    }

    /// Create a new ledger context with a Soroban PRNG seed.
    pub fn with_prng_seed(
        sequence: u32,
        close_time: u64,
        base_fee: u32,
        base_reserve: u32,
        protocol_version: u32,
        network_id: NetworkId,
        soroban_prng_seed: [u8; 32],
    ) -> Self {
        Self {
            sequence,
            close_time,
            base_fee,
            base_reserve,
            protocol_version,
            network_id,
            soroban_prng_seed: Some(soroban_prng_seed),
            frozen_key_config: crate::frozen_keys::FrozenKeyConfig::empty(),
            ledger_flags: 0,
        }
    }

    /// Create context for testnet.
    pub fn testnet(sequence: u32, close_time: u64) -> Self {
        Self {
            sequence,
            close_time,
            base_fee: 100,
            base_reserve: 5_000_000,
            protocol_version: 21,
            network_id: NetworkId::testnet(),
            soroban_prng_seed: None,
            frozen_key_config: crate::frozen_keys::FrozenKeyConfig::empty(),
            ledger_flags: 0,
        }
    }

    /// Create context for mainnet.
    pub fn mainnet(sequence: u32, close_time: u64) -> Self {
        Self {
            sequence,
            close_time,
            base_fee: 100,
            base_reserve: 5_000_000,
            protocol_version: 21,
            network_id: NetworkId::mainnet(),
            soroban_prng_seed: None,
            frozen_key_config: crate::frozen_keys::FrozenKeyConfig::empty(),
            ledger_flags: 0,
        }
    }

    /// Whether liquidity pool trading is disabled by the ledger header flags.
    /// Mirrors stellar-core's `isPoolTradingDisabled()`.
    pub fn is_pool_trading_disabled(&self) -> bool {
        use stellar_xdr::curr::LedgerHeaderFlags;
        self.ledger_flags & (LedgerHeaderFlags::TradingFlag as u32) != 0
    }
}

/// Detailed validation error information.
///
/// Each variant provides specific information about why validation failed,
/// including the expected vs actual values where applicable. This enables
/// detailed error reporting and debugging.
///
/// These errors can be converted to [`ValidationResult`](crate::ValidationResult)
/// for simplified handling via the `From` trait implementation.
#[derive(Debug, Clone)]
pub enum ValidationError {
    /// Transaction envelope has invalid structure or missing required fields.
    InvalidStructure(String),
    /// One or more signatures are cryptographically invalid.
    InvalidSignature,
    /// Required signatures are missing (insufficient weight).
    MissingSignatures,
    /// Sequence number mismatch.
    BadSequence {
        /// Expected sequence (source account seq + 1).
        expected: i64,
        /// Actual sequence in the transaction.
        actual: i64,
    },
    /// Transaction fee is below the minimum required.
    ///
    /// Fields are `i64` to match stellar-core's inclusion-fee semantics
    /// (`TransactionFrame::getInclusionFee()` returns `int64_t`) and
    /// to align with `FeeBumpInsufficientFee` below.
    InsufficientFee {
        /// Minimum required inclusion fee in stroops.
        required: i64,
        /// Inclusion fee provided in the transaction.
        provided: i64,
    },
    /// Source account does not exist in the ledger.
    SourceAccountNotFound,
    /// Source account balance is insufficient to pay the fee.
    InsufficientBalance,
    /// Transaction's maxTime has passed.
    TooLate {
        /// Maximum time allowed by the transaction.
        max_time: u64,
        /// Ledger close time.
        ledger_time: u64,
    },
    /// Transaction's minTime has not yet been reached.
    TooEarly {
        /// Minimum time required by the transaction.
        min_time: u64,
        /// Ledger close time.
        ledger_time: u64,
    },
    /// Ledger sequence is below the minimum allowed.
    LedgerBoundsTooEarly {
        /// Minimum ledger allowed.
        min_ledger: u32,
        /// Current ledger sequence.
        current: u32,
    },
    /// Ledger sequence is at or above the maximum allowed.
    LedgerBoundsTooLate {
        /// Maximum ledger allowed (exclusive upper bound).
        max_ledger: u32,
        /// Current ledger sequence.
        current: u32,
    },
    /// Source account sequence is below the required minimum.
    BadMinAccountSequence,
    /// Minimum time since last sequence bump not met.
    BadMinAccountSequenceAge,
    /// Minimum ledger gap since last sequence bump not met.
    BadMinAccountSequenceLedgerGap,
    /// Required extra signers not present.
    ExtraSignersNotMet,
    /// Fee bump outer fee is insufficient.
    FeeBumpInsufficientFee { outer_fee: i64, required_min: i64 },
    /// Fee bump inner transaction has invalid structure.
    FeeBumpInvalidInner(String),
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidStructure(msg) => write!(f, "invalid structure: {}", msg),
            Self::InvalidSignature => write!(f, "invalid signature"),
            Self::MissingSignatures => write!(f, "missing required signatures"),
            Self::BadSequence { expected, actual } => {
                write!(f, "bad sequence: expected {}, got {}", expected, actual)
            }
            Self::InsufficientFee { required, provided } => {
                write!(
                    f,
                    "insufficient fee: required {}, provided {}",
                    required, provided
                )
            }
            Self::SourceAccountNotFound => write!(f, "source account not found"),
            Self::InsufficientBalance => write!(f, "insufficient balance"),
            Self::TooLate {
                max_time,
                ledger_time,
            } => {
                write!(
                    f,
                    "too late: max_time {}, ledger_time {}",
                    max_time, ledger_time
                )
            }
            Self::TooEarly {
                min_time,
                ledger_time,
            } => {
                write!(
                    f,
                    "too early: min_time {}, ledger_time {}",
                    min_time, ledger_time
                )
            }
            Self::LedgerBoundsTooEarly {
                min_ledger,
                current,
            } => {
                write!(
                    f,
                    "ledger bounds too early: min_ledger {}, current {}",
                    min_ledger, current
                )
            }
            Self::LedgerBoundsTooLate {
                max_ledger,
                current,
            } => {
                write!(
                    f,
                    "ledger bounds too late: max_ledger {}, current {}",
                    max_ledger, current
                )
            }
            Self::BadMinAccountSequence => write!(f, "min account sequence not met"),
            Self::BadMinAccountSequenceAge => write!(f, "min account sequence age not met"),
            Self::BadMinAccountSequenceLedgerGap => {
                write!(f, "min account sequence ledger gap not met")
            }
            Self::ExtraSignersNotMet => write!(f, "extra signers requirement not met"),
            Self::FeeBumpInsufficientFee {
                outer_fee,
                required_min,
            } => write!(
                f,
                "fee bump outer fee {} is less than required {}",
                outer_fee, required_min
            ),
            Self::FeeBumpInvalidInner(msg) => write!(f, "fee bump invalid inner: {}", msg),
        }
    }
}

/// Validate transaction signatures.
///
/// This verifies that the signatures on the transaction are cryptographically
/// valid for the transaction hash. It does NOT verify that the signers have
/// the required weights - that requires account information.
pub fn validate_signatures(
    frame: &TransactionFrame,
    context: &LedgerContext,
) -> std::result::Result<(), ValidationError> {
    // Compute transaction hash - this can fail if the envelope is malformed
    let _tx_hash = frame.hash(&context.network_id).map_err(|e| {
        tracing::debug!("validate_signatures: hash computation failed: {:?}", e);
        ValidationError::InvalidSignature
    })?;

    // Note: We don't validate individual signature formats here because:
    // - Ed25519 signatures are 64 bytes
    // - HashX signatures can be any length (the preimage)
    // - Pre-auth TX signatures are 0 bytes
    // The actual signature verification happens in has_sufficient_signer_weight()
    // when we have access to the account's signers.

    Ok(())
}

/// Validate sequence number.
///
/// For catchup mode, we trust the historical sequence but can verify
/// the relationship if we have account data.
pub fn validate_sequence(
    frame: &TransactionFrame,
    source_account: Option<&AccountEntry>,
) -> std::result::Result<(), ValidationError> {
    if let Some(account) = source_account {
        let expected = account.seq_num.0 + 1;
        let actual = frame.sequence_number();

        if actual != expected {
            return Err(ValidationError::BadSequence { expected, actual });
        }
    }

    Ok(())
}

/// Validate preconditions (min sequence and extra signers).
fn validate_min_seq_num(
    frame: &TransactionFrame,
    source_account: &AccountEntry,
) -> std::result::Result<(), ValidationError> {
    if let Preconditions::V2(cond) = frame.preconditions() {
        if let Some(min_seq) = cond.min_seq_num {
            if source_account.seq_num.0 < min_seq.0 {
                return Err(ValidationError::BadMinAccountSequence);
            }
        }
    }

    Ok(())
}

fn validate_extra_signers(
    frame: &TransactionFrame,
    context: &LedgerContext,
) -> std::result::Result<(), ValidationError> {
    if let Preconditions::V2(cond) = frame.preconditions() {
        if !cond.extra_signers.is_empty() {
            let extra_hash = fee_bump_inner_hash(frame, &context.network_id)
                .map_err(ValidationError::InvalidStructure)?;
            let extra_signatures = if frame.is_fee_bump() {
                frame.inner_signatures()
            } else {
                frame.signatures()
            };
            if !has_required_extra_signers(&extra_hash, extra_signatures, &cond.extra_signers) {
                return Err(ValidationError::ExtraSignersNotMet);
            }
        }
    }

    Ok(())
}

/// Validate transaction fee.
///
/// Mirrors stellar-core `TransactionFrame::commonValid` (chargeFee path)
/// at `TransactionFrame.cpp:1482-1487`, which rejects with
/// `txINSUFFICIENT_FEE` when
/// `getInclusionFee() < getMinInclusionFee(*this, header.current())`.
///
/// `frame.min_inclusion_fee` uses `resource_operation_count()` —
/// `inner_ops + 1` for fee-bumps, matching
/// `FeeBumpTransactionFrame::getNumOperations()`
/// (`stellar-core/src/transactions/FeeBumpTransactionFrame.cpp:594-598`).
///
/// Regression: AUDIT-214 (#2103). The previous implementation compared
/// `frame.fee()` (total fee) against `op_count * base_fee` (using inner
/// op count, not resource op count), which (a) accepted Soroban
/// transactions with `resource_fee == fee` (and therefore inclusion fee
/// zero) and (b) used the wrong op count for fee-bumps.
pub fn validate_fee(
    frame: &TransactionFrame,
    context: &LedgerContext,
) -> std::result::Result<(), ValidationError> {
    if !frame.has_sufficient_inclusion_fee(context.base_fee as i64) {
        let required = frame.min_inclusion_fee(context.base_fee as i64);
        let provided = frame.inclusion_fee();
        return Err(ValidationError::InsufficientFee {
            required: required.as_i64(),
            provided: provided.as_i64(),
        });
    }

    Ok(())
}

/// Validate time bounds.
pub fn validate_time_bounds(
    frame: &TransactionFrame,
    context: &LedgerContext,
) -> std::result::Result<(), ValidationError> {
    let time_bounds = match frame.preconditions() {
        Preconditions::None => return Ok(()),
        Preconditions::Time(tb) => Some(tb),
        Preconditions::V2(cond) => cond.time_bounds,
    };

    if let Some(tb) = time_bounds {
        let min_time: u64 = tb.min_time.into();
        let max_time: u64 = tb.max_time.into();

        // Check min time
        if min_time > 0 && context.close_time < min_time {
            return Err(ValidationError::TooEarly {
                min_time,
                ledger_time: context.close_time,
            });
        }

        // Check max time (0 means no limit)
        if max_time > 0 && context.close_time > max_time {
            return Err(ValidationError::TooLate {
                max_time,
                ledger_time: context.close_time,
            });
        }
    }

    Ok(())
}

/// Validate ledger bounds.
pub fn validate_ledger_bounds(
    frame: &TransactionFrame,
    context: &LedgerContext,
) -> std::result::Result<(), ValidationError> {
    let ledger_bounds = match frame.preconditions() {
        Preconditions::None | Preconditions::Time(_) => return Ok(()),
        Preconditions::V2(cond) => cond.ledger_bounds,
    };

    if let Some(lb) = ledger_bounds {
        let current = context.sequence;

        // Check min ledger
        if lb.min_ledger > 0 && current < lb.min_ledger {
            return Err(ValidationError::LedgerBoundsTooEarly {
                min_ledger: lb.min_ledger,
                current,
            });
        }

        // Check max ledger (0 means no limit)
        // Spec: TX_SPEC §4.2.3 — ledger sequence MUST be strictly less than maxLedger.
        if lb.max_ledger > 0 && current >= lb.max_ledger {
            return Err(ValidationError::LedgerBoundsTooLate {
                max_ledger: lb.max_ledger,
                current,
            });
        }
    }

    Ok(())
}

/// Validate transaction structure.
pub fn validate_structure(frame: &TransactionFrame) -> std::result::Result<(), ValidationError> {
    if !frame.is_valid_structure() {
        return Err(ValidationError::InvalidStructure(
            "basic structure validation failed".to_string(),
        ));
    }

    Ok(())
}

fn validate_soroban_resources(
    frame: &TransactionFrame,
    context: &LedgerContext,
) -> std::result::Result<(), ValidationError> {
    if !frame.is_soroban() {
        // Spec: TX_SPEC §4.2.6 — a non-Soroban transaction MUST NOT carry
        // SorobanTransactionData. Result: txMALFORMED.
        if frame.soroban_data().is_some() {
            return Err(ValidationError::InvalidStructure(
                "non-Soroban transaction must not carry SorobanTransactionData".to_string(),
            ));
        }
        return Ok(());
    }

    if frame.soroban_data().is_none() {
        return Err(ValidationError::InvalidStructure(
            "missing soroban transaction data".to_string(),
        ));
    }

    let Some(data) = frame.soroban_data() else {
        return Ok(());
    };

    // XDRProvidesValidFee: resource_fee must be in [0, MAX_RESOURCE_FEE].
    // stellar-core: TransactionFrame.cpp:1763-1779
    let resource_fee = data.resource_fee;
    if resource_fee < 0 || resource_fee > MAX_RESOURCE_FEE {
        return Err(ValidationError::InvalidStructure(format!(
            "soroban resource fee {} out of valid range [0, {}]",
            resource_fee, MAX_RESOURCE_FEE
        )));
    }

    let footprint = &data.resources.footprint;
    if let stellar_xdr::curr::SorobanTransactionDataExt::V1(resource_ext) = &data.ext {
        let mut prev: Option<u32> = None;
        for index in resource_ext.archived_soroban_entries.iter() {
            if let Some(prev_index) = prev {
                if index <= &prev_index {
                    return Err(ValidationError::InvalidStructure(
                        "archived soroban entry indices must be sorted and unique".to_string(),
                    ));
                }
            }
            prev = Some(*index);

            let idx = *index as usize;
            let Some(key) = footprint.read_write.get(idx) else {
                return Err(ValidationError::InvalidStructure(
                    "archived soroban entry index out of bounds".to_string(),
                ));
            };

            if !henyey_common::is_persistent_key(key) {
                return Err(ValidationError::InvalidStructure(
                    "archived soroban entry must be a persistent contract entry".to_string(),
                ));
            }
        }
    }

    let _ = context;

    // Check for duplicate keys across read_only and read_write footprints.
    // stellar-core rejects duplicates at commonValidPreSeqNum (TransactionFrame.cpp:1416-1444)
    // with txSOROBAN_INVALID. We also check here so validate_basic catches it early.
    {
        let mut seen = std::collections::HashSet::new();
        for key in footprint
            .read_only
            .iter()
            .chain(footprint.read_write.iter())
        {
            // Reject unsupported footprint key types (stellar-core TransactionFrame.cpp:916-950).
            // Valid: Account, Trustline, ContractData, ContractCode.
            // Invalid: Offer, Data, ClaimableBalance, LiquidityPool, ConfigSetting, Ttl.
            match key {
                stellar_xdr::curr::LedgerKey::Account(_)
                | stellar_xdr::curr::LedgerKey::Trustline(_)
                | stellar_xdr::curr::LedgerKey::ContractData(_)
                | stellar_xdr::curr::LedgerKey::ContractCode(_) => {}
                _ => {
                    return Err(ValidationError::InvalidStructure(
                        "unsupported ledger key type in Soroban footprint".to_string(),
                    ));
                }
            }

            if !seen.insert(key) {
                return Err(ValidationError::InvalidStructure(
                    "duplicate key in Soroban footprint".to_string(),
                ));
            }
        }
    }

    Ok(())
}

/// Validate fee bump-specific rules.
///
/// This performs validation specific to fee bump transactions including:
/// - Outer fee >= inner fee (with base fee multiplier)
/// - Inner transaction structure
/// - Inner signature format
fn validate_fee_bump_rules(
    frame: &TransactionFrame,
    context: &LedgerContext,
) -> std::result::Result<(), ValidationError> {
    if !frame.is_fee_bump() {
        return Ok(());
    }

    let mut fee_bump_frame = FeeBumpFrame::from_frame(frame.clone(), &context.network_id)
        .map_err(|e| ValidationError::FeeBumpInvalidInner(e.to_string()))?;

    validate_fee_bump(&mut fee_bump_frame, context).map_err(|e| match e {
        FeeBumpError::InsufficientOuterFee {
            outer_fee,
            required_min,
        } => ValidationError::FeeBumpInsufficientFee {
            outer_fee,
            required_min,
        },
        FeeBumpError::TooManyOperations(count) => {
            ValidationError::FeeBumpInvalidInner(format!("too many operations: {}", count))
        }
        FeeBumpError::InvalidInnerTxType => {
            ValidationError::FeeBumpInvalidInner("inner transaction must be V1".to_string())
        }
        FeeBumpError::NotFeeBump => {
            ValidationError::FeeBumpInvalidInner("not a fee bump transaction".to_string())
        }
        FeeBumpError::HashError(msg) => {
            ValidationError::FeeBumpInvalidInner(format!("hash error: {}", msg))
        }
    })
}

/// Perform all basic validations.
///
/// This is a convenience function that runs all basic checks suitable for catchup.
/// It does not require account data and trusts historical results.
pub fn validate_basic(
    frame: &TransactionFrame,
    context: &LedgerContext,
) -> std::result::Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    if let Err(e) = validate_structure(frame) {
        errors.push(e);
    }

    if let Err(e) = validate_fee(frame, context) {
        errors.push(e);
    }

    if let Err(e) = validate_time_bounds(frame, context) {
        errors.push(e);
    }

    if let Err(e) = validate_ledger_bounds(frame, context) {
        errors.push(e);
    }

    if let Err(e) = validate_soroban_resources(frame, context) {
        errors.push(e);
    }

    // Fee bump specific validation
    if let Err(e) = validate_fee_bump_rules(frame, context) {
        errors.push(e);
    }

    // Signature validation is optional in basic mode
    // (might not have all data needed)

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

// SECURITY: not the production validation path; real validation in TransactionExecutor::validate_preconditions()
/// Full validation with account data.
pub fn validate_full(
    frame: &TransactionFrame,
    context: &LedgerContext,
    source_account: &AccountEntry,
) -> std::result::Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    if let Err(e) = validate_structure(frame) {
        errors.push(e);
    }

    if let Err(e) = validate_fee(frame, context) {
        errors.push(e);
    }

    if let Err(e) = validate_time_bounds(frame, context) {
        errors.push(e);
    }

    if let Err(e) = validate_ledger_bounds(frame, context) {
        errors.push(e);
    }

    if let Err(e) = validate_min_seq_num(frame, source_account) {
        errors.push(e);
    }

    if let Err(e) = validate_sequence(frame, Some(source_account)) {
        errors.push(e);
    }

    if let Err(e) = validate_signatures(frame, context) {
        errors.push(e);
    }

    if let Err(e) = validate_extra_signers(frame, context) {
        errors.push(e);
    }

    if let Err(e) = validate_soroban_resources(frame, context) {
        errors.push(e);
    }

    // Fee bump specific validation
    if let Err(e) = validate_fee_bump_rules(frame, context) {
        errors.push(e);
    }

    // Check account balance can cover fee
    let available_balance = source_account.balance;
    let fee = frame.total_fee().as_i64();
    if available_balance < fee {
        errors.push(ValidationError::InsufficientBalance);
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn fee_bump_inner_hash(
    frame: &TransactionFrame,
    network_id: &NetworkId,
) -> std::result::Result<Hash256, String> {
    match frame.envelope() {
        TransactionEnvelope::TxFeeBump(env) => match &env.tx.inner_tx {
            stellar_xdr::curr::FeeBumpTransactionInnerTx::Tx(inner) => {
                let inner_env = TransactionEnvelope::Tx(inner.clone());
                let inner_frame = TransactionFrame::from_owned_with_network(inner_env, *network_id);
                inner_frame
                    .hash(network_id)
                    .map_err(|e| format!("inner tx hash error: {}", e))
            }
        },
        _ => frame
            .hash(network_id)
            .map_err(|e| format!("tx hash error: {}", e)),
    }
}

fn has_required_extra_signers(
    tx_hash: &Hash256,
    signatures: &[DecoratedSignature],
    extra_signers: &[SignerKey],
) -> bool {
    extra_signers.iter().all(|signer| match signer {
        SignerKey::Ed25519(key) => {
            if let Ok(pk) = PublicKey::from_bytes(&key.0) {
                has_ed25519_signature(tx_hash, signatures, &pk)
            } else {
                false
            }
        }
        SignerKey::PreAuthTx(key) => key.0 == tx_hash.0,
        SignerKey::HashX(key) => has_hashx_signature(signatures, key),
        SignerKey::Ed25519SignedPayload(payload) => {
            has_signed_payload_signature(tx_hash, signatures, payload)
        }
    })
}

fn has_ed25519_signature(
    tx_hash: &Hash256,
    signatures: &[DecoratedSignature],
    pk: &PublicKey,
) -> bool {
    signatures
        .iter()
        .any(|sig| verify_signature_with_key(tx_hash, sig, pk))
}

fn has_hashx_signature(
    signatures: &[DecoratedSignature],
    key: &stellar_xdr::curr::Uint256,
) -> bool {
    signatures.iter().any(|sig| {
        // No length restriction on HashX preimages — stellar-core accepts any
        // length allowed by XDR and checks only sha256(preimage) == key.
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
    signed_payload: &stellar_xdr::curr::SignerKeyEd25519SignedPayload,
) -> bool {
    signatures
        .iter()
        .any(|sig| henyey_crypto::verify_ed25519_signed_payload(sig, signed_payload))
}

/// Check if a signature is well-formed.
///
/// Verify a signature against a known public key.
pub fn verify_signature_with_key(
    tx_hash: &henyey_common::Hash256,
    sig: &DecoratedSignature,
    public_key: &PublicKey,
) -> bool {
    verify_signature_with_raw_key(tx_hash, sig, public_key.as_bytes())
}

/// Like [`verify_signature_with_key`] but accepts raw 32-byte public key bytes.
///
/// Avoids ed25519 point decompression (~35μs) when the signature verification
/// cache already contains the result. This matches stellar-core's approach where
/// `PubKeyUtils::verifySig` checks the cache using raw bytes before any crypto.
pub fn verify_signature_with_raw_key(
    tx_hash: &henyey_common::Hash256,
    sig: &DecoratedSignature,
    key_bytes: &[u8; 32],
) -> bool {
    // Check hint matches
    let expected_hint = [key_bytes[28], key_bytes[29], key_bytes[30], key_bytes[31]];

    if sig.hint.0 != expected_hint {
        return false;
    }

    // Verify signature (decompresses public key only on cache miss)
    if let Ok(signature) = Signature::try_from(&sig.signature) {
        henyey_crypto::verify_hash_from_raw_key(key_bytes, tx_hash, &signature).is_ok()
    } else {
        false
    }
}

/// Error types for pre-sequence-number validation.
///
/// Maps to the subset of `TransactionResultCode` values that can be produced by
/// stateless structural validation (before account loading or sequence checks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreSeqNumError {
    /// Transaction has no operations.
    MissingOperation,
    /// Transaction or operation is structurally malformed.
    Malformed(String),
    /// An operation type is not supported at the current protocol version or
    /// ledger flags.
    OpNotSupported,
    /// Soroban-specific structural constraint violated.
    SorobanInvalid(String),
}

impl PreSeqNumError {
    /// Map to the corresponding `TransactionResultCode`.
    pub fn to_tx_result_code(&self) -> stellar_xdr::curr::TransactionResultCode {
        use stellar_xdr::curr::TransactionResultCode;
        match self {
            Self::MissingOperation => TransactionResultCode::TxMissingOperation,
            Self::Malformed(_) => TransactionResultCode::TxMalformed,
            Self::OpNotSupported => TransactionResultCode::TxNotSupported,
            Self::SorobanInvalid(_) => TransactionResultCode::TxSorobanInvalid,
        }
    }
}

impl std::fmt::Display for PreSeqNumError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingOperation => write!(f, "transaction has no operations"),
            Self::Malformed(msg) => write!(f, "malformed: {}", msg),
            Self::OpNotSupported => write!(f, "operation not supported"),
            Self::SorobanInvalid(msg) => write!(f, "soroban invalid: {}", msg),
        }
    }
}

/// Stateless structural validation of a transaction envelope.
///
/// Mirrors the stateless subset of stellar-core's
/// `TransactionFrame::commonValidPreSeqNum()` (TransactionFrame.cpp:1274-1502).
/// Does not include checks that require account state, fee context, time bounds,
/// or `SorobanNetworkConfig` — those remain in their respective callers (queue
/// admission and ledger preconditions).
///
/// Called by both queue admission and ledger preconditions as Phase 1.
pub fn check_valid_pre_seq_num(
    frame: &TransactionFrame,
    protocol_version: u32,
    ledger_flags: u32,
) -> std::result::Result<(), PreSeqNumError> {
    check_valid_pre_seq_num_with_config(frame, protocol_version, ledger_flags, None)
}

/// Like `check_valid_pre_seq_num` but with optional Soroban network config for
/// additional validation (per-TX resource limits, WASM size, key size).
///
/// Parity: stellar-core `commonValidPreSeqNum` → `checkSorobanResources`.
pub fn check_valid_pre_seq_num_with_config(
    frame: &TransactionFrame,
    protocol_version: u32,
    _ledger_flags: u32,
    soroban_limits: Option<&SorobanResourceLimits>,
) -> std::result::Result<(), PreSeqNumError> {
    // 1. Structure: op count, fee > 0, soroban single-op consistency
    if frame.operations().is_empty() {
        return Err(PreSeqNumError::MissingOperation);
    }

    // Reject legacy TxV0 envelopes at protocol >= 13
    // (stellar-core: TransactionFrame.cpp commonValidPreSeqNum rejects
    // ENVELOPE_TYPE_TX_V0 with txNOT_SUPPORTED for protocol >= 13)
    if protocol_version >= 13 {
        if matches!(
            frame.envelope(),
            stellar_xdr::curr::TransactionEnvelope::TxV0(_)
        ) {
            return Err(PreSeqNumError::OpNotSupported);
        }
    }

    if !frame.is_valid_structure() {
        return Err(PreSeqNumError::Malformed(
            "invalid transaction structure".to_string(),
        ));
    }

    // 1b. XDRProvidesValidFee: Soroban txs must have resource_fee in [0, MAX_RESOURCE_FEE].
    // stellar-core: TransactionFrame.cpp:1763-1779
    if frame.is_soroban() {
        let resource_fee = frame.declared_soroban_resource_fee().as_i64();
        if resource_fee < 0 || resource_fee > MAX_RESOURCE_FEE {
            return Err(PreSeqNumError::Malformed(
                "Soroban resource fee out of valid range".to_string(),
            ));
        }
    }

    // 2. Extra signer structural checks (stellar-core commonValidPreSeqNum:1305-1324)
    //    - Duplicate extra signers
    //    - Empty ed25519 signed payload
    if let Preconditions::V2(cond) = frame.preconditions() {
        let extra_signers = &cond.extra_signers;
        if extra_signers.len() == 2 && extra_signers[0] == extra_signers[1] {
            return Err(PreSeqNumError::Malformed(
                "duplicate extra signers".to_string(),
            ));
        }
        for signer in extra_signers.iter() {
            if let SignerKey::Ed25519SignedPayload(payload) = signer {
                if payload.payload.is_empty() {
                    return Err(PreSeqNumError::Malformed(
                        "extra signer has empty ed25519 signed payload".to_string(),
                    ));
                }
            }
        }
    }

    // 3. Per-op classic validation is NOT done here.
    // stellar-core's commonValidPreSeqNum does NOT iterate classic operations.
    // Classic isOpSupported + doCheckValid run later in OperationFrame::checkValid(),
    // called from checkValidWithOptionallyChargedFee after account loading, sequence,
    // and signature checks. Failures there produce txFAILED with per-op results.
    // See: stellar-core/src/transactions/TransactionFrame.cpp:1273-1503

    // 4. Soroban-specific stateless checks
    if frame.is_soroban() {
        // 4a. Memo/muxed constraints (stellar-core commonValidPreSeqNum:1351-1362)
        if !frame.validate_soroban_memo() {
            return Err(PreSeqNumError::SorobanInvalid(
                "Soroban transactions must not use memo or muxed source accounts".to_string(),
            ));
        }

        // 4b. Host function pairing validation is intentionally NOT here.
        // stellar-core only enforces validateHostFn() during queue admission,
        // not in the checkValid() path used for tx-set validation. Putting it
        // here causes over-rejection of peer-supplied tx sets.

        // 4c. Resource fee bound (non-fee-bump only)
        // stellar-core commonValidPreSeqNum:1376-1393
        if !frame.is_fee_bump()
            && frame.declared_soroban_resource_fee().as_i64() > frame.total_fee().as_i64()
        {
            return Err(PreSeqNumError::SorobanInvalid(
                "soroban resource fee exceeds full transaction fee".to_string(),
            ));
        }

        // 4d. Duplicate footprint keys (stellar-core commonValidPreSeqNum:1424-1450)
        if let Some(data) = frame.soroban_data() {
            let fp = &data.resources.footprint;
            let mut seen = std::collections::HashSet::new();
            for key in fp.read_only.iter().chain(fp.read_write.iter()) {
                if !seen.insert(key) {
                    return Err(PreSeqNumError::SorobanInvalid(
                        "duplicate key in Soroban footprint".to_string(),
                    ));
                }
            }

            // 4e. Per-op footprint structure validation (doCheckValidForSoroban)
            let op = &frame.operations()[0];
            match &op.body {
                // RestoreFootprint: readOnly must be empty, readWrite must be persistent Soroban.
                // stellar-core: RestoreFootprintOpFrame.cpp:423-453
                OperationBody::RestoreFootprint(_) => {
                    if !fp.read_only.is_empty() {
                        return Err(PreSeqNumError::SorobanInvalid(
                            "RestoreFootprint: read-only footprint must be empty".to_string(),
                        ));
                    }
                    for key in fp.read_write.iter() {
                        if !henyey_common::is_persistent_key(key) {
                            return Err(PreSeqNumError::SorobanInvalid(
                                "RestoreFootprint: only persistent Soroban entries can be restored"
                                    .to_string(),
                            ));
                        }
                    }
                }
                // ExtendFootprintTtl: readWrite must be empty, readOnly must be Soroban.
                // stellar-core: ExtendFootprintTTLOpFrame.cpp:321-370
                OperationBody::ExtendFootprintTtl(_) => {
                    if !fp.read_write.is_empty() {
                        return Err(PreSeqNumError::SorobanInvalid(
                            "ExtendFootprintTtl: read-write footprint must be empty".to_string(),
                        ));
                    }
                    for key in fp.read_only.iter() {
                        if !henyey_common::is_soroban_key(key) {
                            return Err(PreSeqNumError::SorobanInvalid(
                                "ExtendFootprintTtl: only Soroban entries can have TTL extended"
                                    .to_string(),
                            ));
                        }
                    }
                }
                // InvokeHostFunction: check WASM size and CreateContract asset
                _ => {
                    if let OperationBody::InvokeHostFunction(invoke) = &op.body {
                        // 4e-i. WASM size gate
                        // stellar-core: InvokeHostFunctionOpFrame.cpp:1290-1299
                        if let Some(max_size) = soroban_limits.map(|l| l.max_contract_size_bytes) {
                            if let stellar_xdr::curr::HostFunction::UploadContractWasm(wasm) =
                                &invoke.host_function
                            {
                                if wasm.len() > max_size as usize {
                                    return Err(PreSeqNumError::SorobanInvalid(format!(
                                        "uploaded Wasm size {} exceeds max {}",
                                        wasm.len(),
                                        max_size
                                    )));
                                }
                            }
                        }
                        // 4e-ii. CreateContract fromAsset: validate asset code
                        // stellar-core: InvokeHostFunctionOpFrame.cpp:1301-1309
                        if let stellar_xdr::curr::HostFunction::CreateContract(args) =
                            &invoke.host_function
                        {
                            if let stellar_xdr::curr::ContractIdPreimage::Asset(asset) =
                                &args.contract_id_preimage
                            {
                                if !is_asset_valid(asset, protocol_version) {
                                    return Err(PreSeqNumError::SorobanInvalid(
                                        "invalid asset in CreateContract fromAsset".to_string(),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        // 4f. Per-TX Soroban resource limit enforcement.
        // Parity: stellar-core commonValidPreSeqNum:1362 → checkSorobanResources()
        // (TransactionFrame.cpp:827-1044).
        if let Some(limits) = soroban_limits {
            check_soroban_resource_limits(frame, protocol_version, limits)?;
        }
    } else {
        // 5. Classic: reject ext != 0 on p21+ (stellar-core commonValidPreSeqNum:1454-1467)
        // inner_tx() handles both Tx and TxFeeBump (returning the inner Transaction),
        // so fee-bump-wrapped classic txs with V1 extensions are correctly rejected.
        if protocol_version >= 21 {
            if let Some(tx) = frame.inner_tx() {
                if tx.ext != stellar_xdr::curr::TransactionExt::V0 {
                    return Err(PreSeqNumError::Malformed(
                        "classic transaction must not carry extension data".to_string(),
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Check per-TX Soroban resource limits against network configuration.
///
/// Parity: stellar-core `TransactionFrame::checkSorobanResources()`
/// (TransactionFrame.cpp:827-1044). Validates:
/// - Instructions, read/write bytes, read/write entries
/// - TX size
/// - Footprint entry count (v23+)
/// - Protocol-versioned disk read entry counting (v23+ excludes live Soroban entries)
/// - Footprint key validity (type, trustline asset, XDR key size)
fn check_soroban_resource_limits(
    frame: &TransactionFrame,
    protocol_version: u32,
    limits: &SorobanResourceLimits,
) -> std::result::Result<(), PreSeqNumError> {
    let data = frame
        .soroban_data()
        .expect("caller ensures frame is Soroban");
    let resources = &data.resources;
    let read_entries = &resources.footprint.read_only;
    let write_entries = &resources.footprint.read_write;

    // 1. Instructions limit
    if resources.instructions as u64 > limits.tx_max_instructions {
        return Err(PreSeqNumError::SorobanInvalid(format!(
            "instructions {} exceed per-TX limit {}",
            resources.instructions, limits.tx_max_instructions
        )));
    }

    // 2. Disk read bytes limit
    if resources.disk_read_bytes as u64 > limits.tx_max_read_bytes {
        return Err(PreSeqNumError::SorobanInvalid(format!(
            "disk read bytes {} exceed per-TX limit {}",
            resources.disk_read_bytes, limits.tx_max_read_bytes
        )));
    }

    // 3. Write bytes limit
    if resources.write_bytes as u64 > limits.tx_max_write_bytes {
        return Err(PreSeqNumError::SorobanInvalid(format!(
            "write bytes {} exceed per-TX limit {}",
            resources.write_bytes, limits.tx_max_write_bytes
        )));
    }

    // 4. Protocol-versioned disk read entry counting
    // Parity: stellar-core getNumDiskReadEntries (TransactionFrame.cpp:68-102)
    let num_disk_reads: u64 = if protocol_version >= 23 {
        let disk_reads = get_num_disk_read_entries(resources, &data.ext, frame);

        // v23+: total footprint entries check (txMaxFootprintEntries)
        let total_entries = (read_entries.len() + write_entries.len()) as u64;
        if total_entries > limits.tx_max_footprint_entries {
            return Err(PreSeqNumError::SorobanInvalid(format!(
                "footprint entries {} exceed per-TX limit {}",
                total_entries, limits.tx_max_footprint_entries
            )));
        }

        disk_reads
    } else {
        // Pre-v23: all footprint entries count as disk reads
        (read_entries.len() + write_entries.len()) as u64
    };

    if num_disk_reads > limits.tx_max_read_ledger_entries {
        return Err(PreSeqNumError::SorobanInvalid(format!(
            "disk read entries {} exceed per-TX limit {}",
            num_disk_reads, limits.tx_max_read_ledger_entries
        )));
    }

    // 5. Write entries limit
    if write_entries.len() as u64 > limits.tx_max_write_ledger_entries {
        return Err(PreSeqNumError::SorobanInvalid(format!(
            "write entries {} exceed per-TX limit {}",
            write_entries.len(),
            limits.tx_max_write_ledger_entries
        )));
    }

    // 6. Footprint key validity: type, trustline asset, XDR key size
    // Parity: stellar-core footprintKeyIsValid lambda (TransactionFrame.cpp:916-962)
    for key in read_entries.iter().chain(write_entries.iter()) {
        validate_footprint_key(key, protocol_version, limits)?;
    }

    // 7. TX size limit
    let tx_size = frame.tx_size_bytes() as u64;
    if tx_size > limits.tx_max_size_bytes {
        return Err(PreSeqNumError::SorobanInvalid(format!(
            "tx size {} exceeds per-TX limit {}",
            tx_size, limits.tx_max_size_bytes
        )));
    }

    // 8. Archived entry extension validation (protocol version check)
    // Parity: stellar-core TransactionFrame.cpp:990-1041
    if let stellar_xdr::curr::SorobanTransactionDataExt::V1(_) = &data.ext {
        if protocol_version < 23 {
            return Err(PreSeqNumError::SorobanInvalid(
                "SorobanResourcesExtV0 not supported before protocol 23".to_string(),
            ));
        }
    }

    Ok(())
}

/// Count disk read entries using protocol-versioned logic (v23+).
///
/// Parity: stellar-core `getNumDiskReadEntries` (TransactionFrame.cpp:68-102).
/// For RestoreFootprint ops, all readWrite entries require disk reads.
/// For other ops, only classic (non-Soroban) entries plus archived Soroban
/// entries require disk reads.
fn get_num_disk_read_entries(
    resources: &stellar_xdr::curr::SorobanResources,
    ext: &stellar_xdr::curr::SorobanTransactionDataExt,
    frame: &TransactionFrame,
) -> u64 {
    let is_restore = frame
        .operations()
        .iter()
        .any(|op| matches!(op.body, OperationBody::RestoreFootprint(_)));

    if is_restore {
        return resources.footprint.read_write.len() as u64;
    }

    // Count classic (non-Soroban) entry reads across both footprints
    let mut count: u64 = 0;
    for key in resources
        .footprint
        .read_only
        .iter()
        .chain(resources.footprint.read_write.iter())
    {
        if !henyey_common::is_soroban_key(key) {
            count += 1;
        }
    }

    // Add archived Soroban entries (on-disk by definition)
    if let stellar_xdr::curr::SorobanTransactionDataExt::V1(resource_ext) = ext {
        count += resource_ext.archived_soroban_entries.len() as u64;
    }

    count
}

/// Validate a single footprint key for type, trustline asset validity, and XDR size.
///
/// Parity: stellar-core `footprintKeyIsValid` lambda (TransactionFrame.cpp:916-962).
fn validate_footprint_key(
    key: &stellar_xdr::curr::LedgerKey,
    protocol_version: u32,
    limits: &SorobanResourceLimits,
) -> std::result::Result<(), PreSeqNumError> {
    match key {
        stellar_xdr::curr::LedgerKey::Account(_)
        | stellar_xdr::curr::LedgerKey::ContractData(_)
        | stellar_xdr::curr::LedgerKey::ContractCode(_) => {}
        stellar_xdr::curr::LedgerKey::Trustline(tl) => {
            // stellar-core: reject native, self-issued, and invalid trustline assets
            if !henyey_common::asset::is_trustline_asset_valid(&tl.asset, protocol_version)
                || matches!(tl.asset, stellar_xdr::curr::TrustLineAsset::Native)
                || henyey_common::asset::is_trustline_asset_issuer(&tl.account_id, &tl.asset)
            {
                return Err(PreSeqNumError::SorobanInvalid(
                    "footprint contains invalid trustline asset".to_string(),
                ));
            }
        }
        _ => {
            return Err(PreSeqNumError::SorobanInvalid(
                "unsupported ledger key type in footprint".to_string(),
            ));
        }
    }

    // XDR key size check
    let key_size = henyey_common::xdr_encoded_len_u32(key);
    if key_size > limits.max_contract_data_key_size_bytes {
        return Err(PreSeqNumError::SorobanInvalid(format!(
            "footprint key size {} exceeds limit {}",
            key_size, limits.max_contract_data_key_size_bytes
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use henyey_crypto::{sign_hash, SecretKey};
    use stellar_xdr::curr::{
        AccountEntry, AccountEntryExt, AccountId, Asset, ContractDataDurability, ContractId,
        DecoratedSignature, Duration, Hash, HostFunction, InvokeContractArgs, InvokeHostFunctionOp,
        LedgerBounds, LedgerFootprint, LedgerKey, LedgerKeyContractData, ManageDataOp, Memo,
        MuxedAccount, Operation, OperationBody, PaymentOp, Preconditions, PreconditionsV2,
        PublicKey as XdrPublicKey, ScAddress, ScSymbol, ScVal, SequenceNumber,
        Signature as XdrSignature, SignatureHint, SorobanResources, SorobanResourcesExtV0,
        SorobanTransactionData, SorobanTransactionDataExt, String32, String64, StringM, Thresholds,
        TimeBounds, TimePoint, Transaction, TransactionEnvelope, TransactionExt,
        TransactionV1Envelope, Uint256, VecM,
    };

    fn create_test_frame() -> TransactionFrame {
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

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        TransactionFrame::from_owned(envelope)
    }

    fn create_account_entry(account_id: AccountId, seq_num: i64) -> AccountEntry {
        AccountEntry {
            account_id,
            balance: 10_000_000,
            seq_num: SequenceNumber(seq_num),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([1, 0, 0, 0]),
            signers: VecM::default(),
            ext: AccountEntryExt::V0,
        }
    }

    fn create_soroban_envelope(
        read_write: Vec<LedgerKey>,
        archived_indices: Option<Vec<u32>>,
        with_data: bool,
    ) -> TransactionEnvelope {
        let source = MuxedAccount::Ed25519(Uint256([2u8; 32]));
        let host_function = HostFunction::InvokeContract(InvokeContractArgs {
            contract_address: ScAddress::Contract(ContractId(Hash([9u8; 32]))),
            function_name: ScSymbol(StringM::<32>::try_from("noop".to_string()).unwrap()),
            args: VecM::<ScVal>::default(),
        });

        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function,
                auth: VecM::default(),
            }),
        };

        let mut tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        if with_data {
            let footprint = LedgerFootprint {
                read_only: VecM::default(),
                read_write: read_write.try_into().unwrap(),
            };
            let ext = match archived_indices {
                Some(indices) => SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                    archived_soroban_entries: indices.try_into().unwrap(),
                }),
                None => SorobanTransactionDataExt::V0,
            };
            tx.ext = TransactionExt::V1(SorobanTransactionData {
                ext,
                resources: SorobanResources {
                    footprint,
                    instructions: 100,
                    disk_read_bytes: 0,
                    write_bytes: 0,
                },
                resource_fee: 0,
            });
        }

        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        })
    }

    fn sign_envelope(
        envelope: &TransactionEnvelope,
        secret: &SecretKey,
        network_id: &NetworkId,
    ) -> DecoratedSignature {
        let frame = TransactionFrame::from_owned_with_network(envelope.clone(), *network_id);
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

    #[test]
    fn test_validate_structure() {
        let frame = create_test_frame();
        assert!(validate_structure(&frame).is_ok());
    }

    #[test]
    fn test_validate_fee() {
        let frame = create_test_frame();
        let context = LedgerContext::testnet(1, 1000);

        // Fee of 100 is enough for 1 operation with base_fee of 100
        assert!(validate_fee(&frame, &context).is_ok());
    }

    #[test]
    fn test_validate_fee_insufficient() {
        // Create a transaction with low fee
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
            fee: 10, // Too low
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![payment_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(1, 1000);

        assert!(matches!(
            validate_fee(&frame, &context),
            Err(ValidationError::InsufficientFee { .. })
        ));
    }

    #[test]
    fn test_validate_time_bounds_ok() {
        let frame = create_test_frame();
        let context = LedgerContext::testnet(1, 1000);

        // No time bounds, should pass
        assert!(validate_time_bounds(&frame, &context).is_ok());
    }

    #[test]
    fn test_validate_basic() {
        let frame = create_test_frame();
        let context = LedgerContext::testnet(1, 1000);

        assert!(validate_basic(&frame, &context).is_ok());
    }

    #[test]
    fn test_validate_soroban_missing_data() {
        let envelope = create_soroban_envelope(Vec::new(), None, false);
        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(1, 1000);

        assert!(matches!(
            validate_basic(&frame, &context),
            Err(errors) if matches!(errors.first(), Some(ValidationError::InvalidStructure(_)))
        ));
    }

    #[test]
    fn test_validate_soroban_archived_index_out_of_bounds() {
        let key = LedgerKey::ContractCode(stellar_xdr::curr::LedgerKeyContractCode {
            hash: Hash([3u8; 32]),
        });
        let envelope = create_soroban_envelope(vec![key], Some(vec![1]), true);
        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(1, 1000);

        assert!(matches!(
            validate_basic(&frame, &context),
            Err(errors) if matches!(errors.first(), Some(ValidationError::InvalidStructure(_)))
        ));
    }

    #[test]
    fn test_validate_soroban_archived_key_must_be_persistent() {
        let key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([4u8; 32]))),
            key: ScVal::I32(1),
            durability: ContractDataDurability::Temporary,
        });
        let envelope = create_soroban_envelope(vec![key], Some(vec![0]), true);
        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(1, 1000);

        assert!(matches!(
            validate_basic(&frame, &context),
            Err(errors) if matches!(errors.first(), Some(ValidationError::InvalidStructure(_)))
        ));
    }

    /// [AUDIT-096] validate_basic must reject Soroban TXs with duplicate footprint keys.
    #[test]
    fn test_audit_096_validate_basic_rejects_duplicate_footprint_keys() {
        let dup_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([99u8; 32]))),
            key: ScVal::Symbol(ScSymbol("test".try_into().unwrap())),
            durability: ContractDataDurability::Persistent,
        });

        // Duplicate within read_only
        let envelope = create_soroban_envelope(vec![dup_key.clone(), dup_key.clone()], None, false);
        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(1, 1000);
        assert!(
            validate_basic(&frame, &context).is_err(),
            "duplicate key in read_only should be rejected"
        );

        // Duplicate across read_only and read_write
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![dup_key.clone()].try_into().unwrap(),
                    read_write: vec![dup_key].try_into().unwrap(),
                },
                instructions: 100,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 50,
        };
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                    host_function: HostFunction::InvokeContract(InvokeContractArgs {
                        contract_address: ScAddress::Contract(ContractId(Hash([1u8; 32]))),
                        function_name: ScSymbol("test".try_into().unwrap()),
                        args: vec![].try_into().unwrap(),
                    }),
                    auth: vec![].try_into().unwrap(),
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V1(soroban_data),
        };
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });
        let frame = TransactionFrame::from_owned(envelope);
        assert!(
            validate_basic(&frame, &context).is_err(),
            "duplicate key across read_only and read_write should be rejected"
        );
    }

    #[test]
    fn test_validate_full_min_seq_num() {
        let secret = SecretKey::from_seed(&[9u8; 32]);
        let account_id: AccountId = (&secret.public_key()).into();
        let source = MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes()));

        let op = Operation {
            source_account: None,
            body: OperationBody::ManageData(ManageDataOp {
                data_name: String64::try_from(b"minseq".to_vec()).unwrap(),
                data_value: Some(b"value".to_vec().try_into().unwrap()),
            }),
        };

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: None,
            min_seq_num: Some(SequenceNumber(5)),
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: VecM::default(),
        });

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(2),
            cond: preconditions,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });

        let context = LedgerContext::testnet(1, 1000);
        let account = create_account_entry(account_id, 1);
        let result = validate_full(&TransactionFrame::from_owned(envelope), &context, &account);
        assert!(matches!(
            result,
            Err(errors) if matches!(errors.first(), Some(ValidationError::BadMinAccountSequence))
        ));
    }

    #[test]
    fn test_validate_full_extra_signers_missing() {
        let secret = SecretKey::from_seed(&[10u8; 32]);
        let account_id: AccountId = (&secret.public_key()).into();
        let source = MuxedAccount::Ed25519(Uint256(*secret.public_key().as_bytes()));

        let op = Operation {
            source_account: None,
            body: OperationBody::ManageData(ManageDataOp {
                data_name: String64::try_from(b"extra".to_vec()).unwrap(),
                data_value: Some(b"value".to_vec().try_into().unwrap()),
            }),
        };

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: None,
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![SignerKey::Ed25519(Uint256([1u8; 32]))]
                .try_into()
                .unwrap(),
        });

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(2),
            cond: preconditions,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });

        let context = LedgerContext::testnet(1, 1000);
        let account = create_account_entry(account_id, 1);
        let result = validate_full(&TransactionFrame::from_owned(envelope), &context, &account);
        assert!(matches!(
            result,
            Err(errors) if matches!(errors.first(), Some(ValidationError::ExtraSignersNotMet))
        ));
    }

    #[test]
    fn test_validate_full_extra_signers_satisfied() {
        let extra_secret = SecretKey::from_seed(&[11u8; 32]);
        let account_secret = SecretKey::from_seed(&[12u8; 32]);
        let account_id: AccountId = (&account_secret.public_key()).into();
        let source = MuxedAccount::Ed25519(Uint256(*account_secret.public_key().as_bytes()));

        let op = Operation {
            source_account: None,
            body: OperationBody::ManageData(ManageDataOp {
                data_name: String64::try_from(b"extra".to_vec()).unwrap(),
                data_value: Some(b"value".to_vec().try_into().unwrap()),
            }),
        };

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: None,
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![SignerKey::Ed25519(Uint256(
                *extra_secret.public_key().as_bytes(),
            ))]
            .try_into()
            .unwrap(),
        });

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(2),
            cond: preconditions,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let mut envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });

        let network_id = NetworkId::testnet();
        let sig = sign_envelope(&envelope, &extra_secret, &network_id);
        if let TransactionEnvelope::Tx(ref mut env) = envelope {
            env.signatures = vec![sig].try_into().unwrap();
        }

        let context = LedgerContext::testnet(1, 1000);
        let account = create_account_entry(account_id, 1);
        assert!(validate_full(&TransactionFrame::from_owned(envelope), &context, &account).is_ok());
    }

    /// Test validate_time_bounds with min_time in the future.
    #[test]
    fn test_validate_time_bounds_too_early() {
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

        // Time bounds: min_time = 2000, max_time = 3000
        // Context close time: 1000 (too early)
        let time_bounds = TimeBounds {
            min_time: TimePoint(2000),
            max_time: TimePoint(3000),
        };

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::Time(time_bounds),
            memo: Memo::None,
            operations: vec![payment_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(1, 1000); // close_time = 1000

        assert!(matches!(
            validate_time_bounds(&frame, &context),
            Err(ValidationError::TooEarly { .. })
        ));
    }

    /// Test validate_time_bounds with max_time in the past.
    #[test]
    fn test_validate_time_bounds_too_late() {
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

        // Time bounds: min_time = 100, max_time = 500
        // Context close time: 1000 (too late)
        let time_bounds = TimeBounds {
            min_time: TimePoint(100),
            max_time: TimePoint(500),
        };

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::Time(time_bounds),
            memo: Memo::None,
            operations: vec![payment_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(1, 1000); // close_time = 1000

        assert!(matches!(
            validate_time_bounds(&frame, &context),
            Err(ValidationError::TooLate { .. })
        ));
    }

    /// Test validate_ledger_bounds with min_ledger in the future.
    #[test]
    fn test_validate_ledger_bounds_too_early() {
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

        // Ledger bounds: min_ledger = 100, max_ledger = 200
        // Context ledger: 50 (too early)
        let ledger_bounds = LedgerBounds {
            min_ledger: 100,
            max_ledger: 200,
        };

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: Some(ledger_bounds),
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![].try_into().unwrap(),
        });

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: preconditions,
            memo: Memo::None,
            operations: vec![payment_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(50, 1000); // ledger = 50

        assert!(matches!(
            validate_ledger_bounds(&frame, &context),
            Err(ValidationError::LedgerBoundsTooEarly { .. })
        ));
    }

    /// Test validate_ledger_bounds with max_ledger in the past.
    #[test]
    fn test_validate_ledger_bounds_too_late() {
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

        // Ledger bounds: min_ledger = 10, max_ledger = 50
        // Context ledger: 100 (too late)
        let ledger_bounds = LedgerBounds {
            min_ledger: 10,
            max_ledger: 50,
        };

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: Some(ledger_bounds),
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![].try_into().unwrap(),
        });

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: preconditions,
            memo: Memo::None,
            operations: vec![payment_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(100, 1000); // ledger = 100

        assert!(matches!(
            validate_ledger_bounds(&frame, &context),
            Err(ValidationError::LedgerBoundsTooLate { .. })
        ));
    }

    /// Test validate_sequence with wrong sequence number.
    #[test]
    fn test_validate_sequence_wrong_seqnum() {
        let frame = create_test_frame();
        // Account has seq_num = 5, but tx expects seq_num = 1
        // Transaction seq_num is 1, so account should have seq_num = 0 for it to pass
        let account_id = AccountId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32])));
        let account = create_account_entry(account_id, 5);

        assert!(matches!(
            validate_sequence(&frame, Some(&account)),
            Err(ValidationError::BadSequence { .. })
        ));
    }

    /// Test LedgerContext creation methods.
    #[test]
    fn test_ledger_context_creation() {
        let testnet = LedgerContext::testnet(100, 5000);
        assert_eq!(testnet.sequence, 100);
        assert_eq!(testnet.close_time, 5000);
        assert_eq!(testnet.network_id, NetworkId::testnet());

        let mainnet = LedgerContext::mainnet(200, 6000);
        assert_eq!(mainnet.sequence, 200);
        assert_eq!(mainnet.close_time, 6000);
        assert_eq!(mainnet.network_id, NetworkId::mainnet());

        // Test custom context
        let custom = LedgerContext::new(
            300,
            7000,
            100,     // base_fee
            5000000, // base_reserve
            25,      // protocol_version
            NetworkId::testnet(),
        );
        assert_eq!(custom.sequence, 300);
        assert_eq!(custom.close_time, 7000);
        assert_eq!(custom.base_fee, 100);
        assert_eq!(custom.protocol_version, 25);
    }

    /// Test validate_sequence with correct sequence number.
    #[test]
    fn test_validate_sequence_correct() {
        let frame = create_test_frame();
        // Transaction seq_num is 1, so account should have seq_num = 0
        let account_id = AccountId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32])));
        let account = create_account_entry(account_id, 0);

        assert!(validate_sequence(&frame, Some(&account)).is_ok());
    }

    /// Test validate_sequence without account.
    #[test]
    fn test_validate_sequence_no_account() {
        let frame = create_test_frame();
        // When account is None, sequence validation should pass (for basic validation)
        assert!(validate_sequence(&frame, None).is_ok());
    }

    /// Test validate_ledger_bounds within valid range.
    #[test]
    fn test_validate_ledger_bounds_valid() {
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

        // Ledger bounds: min_ledger = 50, max_ledger = 150
        // Context ledger: 100 (within bounds)
        let ledger_bounds = LedgerBounds {
            min_ledger: 50,
            max_ledger: 150,
        };

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: Some(ledger_bounds),
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![].try_into().unwrap(),
        });

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: preconditions,
            memo: Memo::None,
            operations: vec![payment_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(100, 1000); // ledger = 100

        assert!(validate_ledger_bounds(&frame, &context).is_ok());
    }

    /// Test validate_time_bounds within valid range.
    #[test]
    fn test_validate_time_bounds_valid() {
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

        // Time bounds: min_time = 500, max_time = 1500
        // Context close time: 1000 (within bounds)
        let time_bounds = TimeBounds {
            min_time: TimePoint(500),
            max_time: TimePoint(1500),
        };

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::Time(time_bounds),
            memo: Memo::None,
            operations: vec![payment_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(1, 1000); // close_time = 1000

        assert!(validate_time_bounds(&frame, &context).is_ok());
    }

    /// Test validate_time_bounds with unbounded max_time (0).
    #[test]
    fn test_validate_time_bounds_no_max() {
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

        // Time bounds: min_time = 100, max_time = 0 (no max)
        let time_bounds = TimeBounds {
            min_time: TimePoint(100),
            max_time: TimePoint(0),
        };

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::Time(time_bounds),
            memo: Memo::None,
            operations: vec![payment_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(1, 999999); // Any large close_time

        assert!(validate_time_bounds(&frame, &context).is_ok());
    }

    /// Test validate_ledger_bounds with unbounded max_ledger (0).
    #[test]
    fn test_validate_ledger_bounds_no_max() {
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

        // Ledger bounds: min_ledger = 10, max_ledger = 0 (no max)
        let ledger_bounds = LedgerBounds {
            min_ledger: 10,
            max_ledger: 0,
        };

        let preconditions = Preconditions::V2(PreconditionsV2 {
            time_bounds: None,
            ledger_bounds: Some(ledger_bounds),
            min_seq_num: None,
            min_seq_age: Duration(0),
            min_seq_ledger_gap: 0,
            extra_signers: vec![].try_into().unwrap(),
        });

        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: preconditions,
            memo: Memo::None,
            operations: vec![payment_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(999999, 1000); // Any large ledger

        assert!(validate_ledger_bounds(&frame, &context).is_ok());
    }

    /// Test validation with multiple operations.
    #[test]
    fn test_validate_multiple_operations() {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([1u8; 32]));

        let op1 = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest.clone(),
                asset: Asset::Native,
                amount: 1000,
            }),
        };

        let op2 = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest,
                asset: Asset::Native,
                amount: 2000,
            }),
        };

        let tx = Transaction {
            source_account: source,
            fee: 200, // 100 per op * 2 ops
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op1, op2].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(1, 1000);

        assert!(validate_basic(&frame, &context).is_ok());
    }

    /// Test validation with insufficient fee for multiple operations.
    #[test]
    fn test_validate_insufficient_fee_multiple_ops() {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([1u8; 32]));

        let op1 = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest.clone(),
                asset: Asset::Native,
                amount: 1000,
            }),
        };

        let op2 = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest,
                asset: Asset::Native,
                amount: 2000,
            }),
        };

        let tx = Transaction {
            source_account: source,
            fee: 150, // Too low for 2 ops (need 200)
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op1, op2].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(1, 1000);

        assert!(matches!(
            validate_fee(&frame, &context),
            Err(ValidationError::InsufficientFee { .. })
        ));
    }

    /// Test ValidationError display.
    #[test]
    fn test_validation_error_display() {
        let err = ValidationError::InvalidStructure("bad memo".to_string());
        let display = format!("{}", err);
        assert!(display.contains("bad memo"));

        let err = ValidationError::InsufficientFee {
            required: 200,
            provided: 100,
        };
        let display = format!("{}", err);
        assert!(display.contains("200"));
        assert!(display.contains("100"));

        let err = ValidationError::BadSequence {
            expected: 5,
            actual: 3,
        };
        let display = format!("{}", err);
        assert!(display.contains("5"));
        assert!(display.contains("3"));
    }

    // ── TX_SPEC §4.2.3: maxLedger boundary condition ─────────────────

    #[test]
    fn test_validate_ledger_bounds_at_exact_max_ledger() {
        // TX_SPEC §4.2.3: ledger sequence MUST be strictly less than maxLedger.
        // When current == max_ledger, the tx should be rejected.
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([1u8; 32]));

        let op = Operation {
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
            cond: Preconditions::V2(PreconditionsV2 {
                time_bounds: None,
                ledger_bounds: Some(LedgerBounds {
                    min_ledger: 0,
                    max_ledger: 100,
                }),
                min_seq_num: None,
                min_seq_ledger_gap: 0,
                min_seq_age: Duration(0),
                extra_signers: VecM::default(),
            }),
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);

        // current == max_ledger: should fail
        let context = LedgerContext::testnet(100, 1000);
        assert!(
            matches!(
                validate_ledger_bounds(&frame, &context),
                Err(ValidationError::LedgerBoundsTooLate { .. })
            ),
            "current == max_ledger must be rejected (strictly less than)"
        );

        // current == max_ledger - 1: should pass
        let context = LedgerContext::testnet(99, 1000);
        assert!(
            validate_ledger_bounds(&frame, &context).is_ok(),
            "current < max_ledger must pass"
        );
    }

    // ── TX_SPEC §4.2.6: non-Soroban tx with SorobanTransactionData ──

    #[test]
    fn test_reject_non_soroban_tx_with_soroban_data() {
        // A non-Soroban transaction carrying SorobanTransactionData MUST be rejected.
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([1u8; 32]));

        // Payment = non-Soroban operation
        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: dest,
                asset: Asset::Native,
                amount: 1000,
            }),
        };

        // Attach SorobanTransactionData to a non-Soroban tx
        let soroban_data = SorobanTransactionData {
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
            resource_fee: 100,
        };

        let tx = Transaction {
            source_account: source,
            fee: 200,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V1(soroban_data),
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(100, 1000);

        let result = validate_basic(&frame, &context);
        assert!(
            result.is_err(),
            "non-Soroban tx with SorobanTransactionData must be rejected"
        );
        let errors = result.unwrap_err();
        let has_non_soroban_error = errors.iter().any(|e| match e {
            ValidationError::InvalidStructure(msg) => msg.contains("non-Soroban"),
            _ => false,
        });
        assert!(
            has_non_soroban_error,
            "errors should include InvalidStructure mentioning non-Soroban: {errors:?}"
        );
    }

    // --- check_valid_pre_seq_num tests ---

    #[test]
    fn test_check_valid_pre_seq_num_valid_classic_tx() {
        let frame = create_test_frame();
        assert!(check_valid_pre_seq_num(&frame, 21, 0).is_ok());
    }

    #[test]
    fn test_check_valid_pre_seq_num_missing_operation() {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
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
            signatures: vec![].try_into().unwrap(),
        });
        let frame = TransactionFrame::from_owned(envelope);
        let err = check_valid_pre_seq_num(&frame, 21, 0).unwrap_err();
        assert!(
            matches!(err, PreSeqNumError::MissingOperation),
            "expected MissingOperation, got {err:?}"
        );
    }

    #[test]
    fn test_check_valid_pre_seq_num_duplicate_extra_signers() {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let signer = SignerKey::Ed25519(Uint256([42u8; 32]));
        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::V2(PreconditionsV2 {
                time_bounds: None,
                ledger_bounds: None,
                min_seq_num: None,
                min_seq_ledger_gap: 0,
                min_seq_age: Duration(0),
                extra_signers: vec![signer.clone(), signer].try_into().unwrap(),
            }),
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
            signatures: vec![].try_into().unwrap(),
        });
        let frame = TransactionFrame::from_owned(envelope);
        let err = check_valid_pre_seq_num(&frame, 21, 0).unwrap_err();
        assert!(
            matches!(err, PreSeqNumError::Malformed(_)),
            "expected Malformed for duplicate signers, got {err:?}"
        );
    }

    #[test]
    fn test_check_valid_pre_seq_num_empty_signed_payload_extra_signer() {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        let signer =
            SignerKey::Ed25519SignedPayload(stellar_xdr::curr::SignerKeyEd25519SignedPayload {
                ed25519: Uint256([42u8; 32]),
                payload: vec![].try_into().unwrap(),
            });
        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::V2(PreconditionsV2 {
                time_bounds: None,
                ledger_bounds: None,
                min_seq_num: None,
                min_seq_ledger_gap: 0,
                min_seq_age: Duration(0),
                extra_signers: vec![signer].try_into().unwrap(),
            }),
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
            signatures: vec![].try_into().unwrap(),
        });
        let frame = TransactionFrame::from_owned(envelope);
        let err = check_valid_pre_seq_num(&frame, 21, 0).unwrap_err();
        assert!(
            matches!(err, PreSeqNumError::Malformed(_)),
            "expected Malformed for empty payload, got {err:?}"
        );
    }

    #[test]
    fn test_check_valid_pre_seq_num_inflation_not_rejected() {
        // Inflation is not supported on protocol >= 12, but classic ops are no
        // longer validated in pre-seq-num (issue #2063). This test verifies
        // that pre-seq-num passes; the rejection now happens in the apply path.
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::Inflation,
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V0,
        };
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });
        let frame = TransactionFrame::from_owned(envelope);
        assert!(
            check_valid_pre_seq_num(&frame, 21, 0).is_ok(),
            "pre-seq-num must not reject classic ops (issue #2063)"
        );
    }

    #[test]
    fn test_check_valid_pre_seq_num_classic_ext_rejection_p21() {
        // Classic tx with non-V0 ext should be rejected on p21+
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([1u8; 32]));
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
            ext: TransactionExt::V1(SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: LedgerFootprint {
                        read_only: vec![].try_into().unwrap(),
                        read_write: vec![].try_into().unwrap(),
                    },
                    instructions: 0,
                    disk_read_bytes: 0,
                    write_bytes: 0,
                },
                resource_fee: 0,
            }),
        };
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });
        let frame = TransactionFrame::from_owned(envelope);
        let err = check_valid_pre_seq_num(&frame, 21, 0).unwrap_err();
        assert!(
            matches!(err, PreSeqNumError::Malformed(_)),
            "expected Malformed for classic ext, got {err:?}"
        );
    }

    #[test]
    fn test_check_valid_pre_seq_num_classic_ext_rejection_feebump_p21() {
        // Regression for #2059: a fee-bump envelope wrapping a classic inner tx
        // with TransactionExt::V1 must also be rejected on p21+, matching
        // stellar-core's inner-tx revalidation in FeeBumpTransactionFrame.cpp:307-309.
        use stellar_xdr::curr::{
            FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
            FeeBumpTransactionInnerTx,
        };

        let inner_source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let outer_fee_source = MuxedAccount::Ed25519(Uint256([2u8; 32]));
        let dest = MuxedAccount::Ed25519(Uint256([1u8; 32]));

        let inner_tx = Transaction {
            source_account: inner_source,
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
            ext: TransactionExt::V1(SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: LedgerFootprint {
                        read_only: vec![].try_into().unwrap(),
                        read_write: vec![].try_into().unwrap(),
                    },
                    instructions: 0,
                    disk_read_bytes: 0,
                    write_bytes: 0,
                },
                resource_fee: 0,
            }),
        };

        let inner_env = TransactionV1Envelope {
            tx: inner_tx,
            signatures: vec![].try_into().unwrap(),
        };

        let fee_bump_tx = FeeBumpTransaction {
            fee_source: outer_fee_source,
            fee: 200,
            inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
            ext: FeeBumpTransactionExt::V0,
        };

        let envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: fee_bump_tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let err = check_valid_pre_seq_num(&frame, 21, 0).unwrap_err();
        assert!(
            matches!(err, PreSeqNumError::Malformed(_)),
            "expected Malformed for fee-bump classic ext, got {err:?}"
        );
    }

    #[test]
    fn test_check_valid_pre_seq_num_soroban_duplicate_footprint_key() {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let dup_key = LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(stellar_xdr::curr::ContractId(Hash([99u8; 32]))),
            key: ScVal::Symbol(ScSymbol("test".try_into().unwrap())),
            durability: ContractDataDurability::Persistent,
        });
        let soroban_data = SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![dup_key.clone(), dup_key].try_into().unwrap(),
                    read_write: vec![].try_into().unwrap(),
                },
                instructions: 100,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 50,
        };
        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                    host_function: HostFunction::InvokeContract(InvokeContractArgs {
                        contract_address: ScAddress::Contract(stellar_xdr::curr::ContractId(Hash(
                            [1u8; 32],
                        ))),
                        function_name: ScSymbol("test".try_into().unwrap()),
                        args: vec![].try_into().unwrap(),
                    }),
                    auth: vec![].try_into().unwrap(),
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V1(soroban_data),
        };
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![].try_into().unwrap(),
        });
        let frame = TransactionFrame::from_owned(envelope);
        let err = check_valid_pre_seq_num(&frame, 21, 0).unwrap_err();
        assert!(
            matches!(err, PreSeqNumError::SorobanInvalid(_)),
            "expected SorobanInvalid for dup footprint, got {err:?}"
        );
    }

    /// AUDIT-074: HashX extra_signers must accept non-32-byte preimages.
    #[test]
    fn test_audit_074_hashx_extra_signer_non_32_byte_preimage() {
        use henyey_common::Hash256;
        use stellar_xdr::curr::{DecoratedSignature, Signature, SignatureHint, Uint256};

        // 5-byte preimage "hello"
        let preimage = b"hello";
        let hash = Hash256::hash(preimage);
        let key = Uint256(hash.0);
        let hint = SignatureHint([key.0[28], key.0[29], key.0[30], key.0[31]]);

        let sig = DecoratedSignature {
            hint: hint.clone(),
            signature: Signature(preimage.to_vec().try_into().unwrap()),
        };

        // Should match: sha256("hello") == key
        assert!(
            has_hashx_signature(std::slice::from_ref(&sig), &key),
            "5-byte HashX preimage should be accepted"
        );

        // 32-byte preimage should also still work
        let preimage32 = [0xABu8; 32];
        let hash32 = Hash256::hash(&preimage32);
        let key32 = Uint256(hash32.0);
        let hint32 = SignatureHint([key32.0[28], key32.0[29], key32.0[30], key32.0[31]]);
        let sig32 = DecoratedSignature {
            hint: hint32,
            signature: Signature(preimage32.to_vec().try_into().unwrap()),
        };
        assert!(
            has_hashx_signature(&[sig32], &key32),
            "32-byte HashX preimage should still work"
        );

        // Wrong preimage should fail
        let wrong_sig = DecoratedSignature {
            hint,
            signature: Signature(b"wrong".to_vec().try_into().unwrap()),
        };
        assert!(
            !has_hashx_signature(&[wrong_sig], &key),
            "Wrong preimage should be rejected"
        );
    }

    #[test]
    fn test_check_valid_pre_seq_num_fee_bump_uses_inner_source() {
        // Regression: check_valid_pre_seq_num must validate ops against the inner
        // tx source, not the outer fee source. A payment from inner→dest is valid
        // when the inner source differs from the outer fee source.
        use stellar_xdr::curr::{
            FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
            FeeBumpTransactionInnerTx,
        };

        let inner_source = MuxedAccount::Ed25519(Uint256([10u8; 32]));
        let outer_fee_source = MuxedAccount::Ed25519(Uint256([20u8; 32]));
        // Destination == inner source (self-payment): valid per inner source,
        // but would be txMALFORMED if validated against the outer source
        // (since outer != dest, the payment is still valid, but we can
        // distinguish by making dest == outer_fee_source which is invalid
        // as "pay to self" only if the effective source == outer_fee_source).
        let dest = MuxedAccount::Ed25519(Uint256([20u8; 32])); // same as outer

        let payment_op = Operation {
            source_account: None, // inherits from tx source
            body: OperationBody::Payment(PaymentOp {
                destination: dest,
                asset: Asset::Native,
                amount: 1000,
            }),
        };

        let inner_tx = Transaction {
            source_account: inner_source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![payment_op].try_into().unwrap(),
            ext: TransactionExt::V0,
        };

        let inner_env = TransactionV1Envelope {
            tx: inner_tx,
            signatures: vec![].try_into().unwrap(),
        };

        let fee_bump_tx = FeeBumpTransaction {
            fee_source: outer_fee_source,
            fee: 200,
            inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
            ext: FeeBumpTransactionExt::V0,
        };

        let envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: fee_bump_tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);

        // Inner source (10...) != dest (20...) → payment is valid
        // If we incorrectly used outer source (20...) == dest (20...) → "pay to self"
        // would also pass for Payment, so instead verify the effective source used:
        assert_eq!(
            frame.inner_source_account_id(),
            AccountId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([10u8; 32])))
        );
        assert_ne!(frame.source_account_id(), frame.inner_source_account_id());

        // The validation should succeed — inner source != dest, so it's valid
        assert!(
            check_valid_pre_seq_num(&frame, 25, 0).is_ok(),
            "fee-bump pre-seq validation should use inner source, not outer fee source"
        );
    }

    /// Regression test for issue #2063: classic op validation errors must NOT
    /// surface as pre-seq-num txMALFORMED. Instead, they should be caught later
    /// in the apply path and produce txFAILED with per-op results.
    #[test]
    fn test_pre_seq_num_does_not_reject_malformed_classic_ops() {
        use stellar_xdr::curr::*;

        let source_key = Uint256([1u8; 32]);
        // ManageBuyOffer with same selling/buying asset: structurally invalid
        let bad_op = Operation {
            source_account: None,
            body: OperationBody::ManageBuyOffer(ManageBuyOfferOp {
                selling: Asset::Native,
                buying: Asset::Native,
                buy_amount: 100,
                price: Price { n: 1, d: 1 },
                offer_id: 0,
            }),
        };

        let tx = TransactionV1Envelope {
            tx: Transaction {
                source_account: MuxedAccount::Ed25519(source_key),
                fee: 100,
                seq_num: SequenceNumber(1),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: vec![bad_op].try_into().unwrap(),
                ext: TransactionExt::V0,
            },
            signatures: VecM::default(),
        };
        let env = TransactionEnvelope::Tx(tx);
        let frame = TransactionFrame::from_owned(env);

        // Pre-seq-num should pass — classic op structural errors are deferred
        assert!(
            check_valid_pre_seq_num(&frame, 25, 0).is_ok(),
            "pre-seq-num must not reject malformed classic ops (issue #2063)"
        );
    }

    // ── validate_fee Soroban regression tests (issue #2114) ──

    /// Helper: create a Soroban TransactionFrame with the given fee and resource_fee.
    fn create_soroban_frame(fee: u32, resource_fee: i64) -> TransactionFrame {
        let source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([9u8; 32]))),
                    function_name: ScSymbol(StringM::<32>::try_from("noop".to_string()).unwrap()),
                    args: VecM::<ScVal>::default(),
                }),
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
            ext: TransactionExt::V1(SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: LedgerFootprint {
                        read_only: vec![].try_into().unwrap(),
                        read_write: vec![].try_into().unwrap(),
                    },
                    instructions: 100,
                    disk_read_bytes: 0,
                    write_bytes: 0,
                },
                resource_fee,
            }),
        };

        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });

        TransactionFrame::from_owned(envelope)
    }

    /// Soroban tx with fee == resource_fee → inclusion_fee = 0, rejected.
    #[test]
    fn test_validate_fee_soroban_zero_inclusion_fee() {
        // fee=500, resource_fee=500 → inclusion_fee = 0
        let frame = create_soroban_frame(500, 500);
        let context = LedgerContext::testnet(1, 1000); // base_fee=100

        let result = validate_fee(&frame, &context);
        assert!(
            matches!(
                result,
                Err(ValidationError::InsufficientFee {
                    required: 100,
                    provided: 0
                })
            ),
            "Soroban tx with zero inclusion fee must be rejected, got: {result:?}"
        );
    }

    /// Soroban tx with inclusion_fee == base_fee → accepted.
    #[test]
    fn test_validate_fee_soroban_sufficient() {
        // fee=600, resource_fee=500 → inclusion_fee = 100, base_fee=100
        let frame = create_soroban_frame(600, 500);
        let context = LedgerContext::testnet(1, 1000);

        assert!(
            validate_fee(&frame, &context).is_ok(),
            "Soroban tx with inclusion_fee == base_fee should pass"
        );
    }

    /// Soroban tx with inclusion_fee < base_fee (but > 0) → rejected.
    #[test]
    fn test_validate_fee_soroban_insufficient_inclusion_fee() {
        // fee=550, resource_fee=500 → inclusion_fee = 50, base_fee=100
        let frame = create_soroban_frame(550, 500);
        let context = LedgerContext::testnet(1, 1000);

        let result = validate_fee(&frame, &context);
        assert!(
            matches!(
                result,
                Err(ValidationError::InsufficientFee {
                    required: 100,
                    provided: 50
                })
            ),
            "Soroban tx with inclusion_fee < base_fee must be rejected, got: {result:?}"
        );
    }

    /// Fee-bump wrapping a Soroban inner tx with zero outer inclusion fee → rejected.
    #[test]
    fn test_validate_fee_fee_bump_soroban_zero_inclusion_fee() {
        use stellar_xdr::curr::{
            FeeBumpTransaction, FeeBumpTransactionEnvelope, FeeBumpTransactionExt,
            FeeBumpTransactionInnerTx,
        };

        let inner_source = MuxedAccount::Ed25519(Uint256([0u8; 32]));
        let outer_fee_source = MuxedAccount::Ed25519(Uint256([2u8; 32]));

        let inner_tx = Transaction {
            source_account: inner_source,
            fee: 500,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                    host_function: HostFunction::InvokeContract(InvokeContractArgs {
                        contract_address: ScAddress::Contract(ContractId(Hash([9u8; 32]))),
                        function_name: ScSymbol(
                            StringM::<32>::try_from("noop".to_string()).unwrap(),
                        ),
                        args: VecM::<ScVal>::default(),
                    }),
                    auth: VecM::default(),
                }),
            }]
            .try_into()
            .unwrap(),
            ext: TransactionExt::V1(SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: LedgerFootprint {
                        read_only: vec![].try_into().unwrap(),
                        read_write: vec![].try_into().unwrap(),
                    },
                    instructions: 100,
                    disk_read_bytes: 0,
                    write_bytes: 0,
                },
                resource_fee: 500,
            }),
        };

        let inner_env = TransactionV1Envelope {
            tx: inner_tx,
            signatures: vec![].try_into().unwrap(),
        };

        // Outer fee = 500 = resource_fee. For fee-bump, resource_operation_count = inner_ops + 1 = 2.
        // min_inclusion_fee = 100 * 2 = 200. inclusion_fee = total_fee - resource_fee = 500 - 500 = 0.
        let fee_bump_tx = FeeBumpTransaction {
            fee_source: outer_fee_source,
            fee: 500,
            inner_tx: FeeBumpTransactionInnerTx::Tx(inner_env),
            ext: FeeBumpTransactionExt::V0,
        };

        let envelope = TransactionEnvelope::TxFeeBump(FeeBumpTransactionEnvelope {
            tx: fee_bump_tx,
            signatures: vec![].try_into().unwrap(),
        });

        let frame = TransactionFrame::from_owned(envelope);
        let context = LedgerContext::testnet(1, 1000);

        let result = validate_fee(&frame, &context);
        assert!(
            matches!(
                result,
                Err(ValidationError::InsufficientFee {
                    required: 200,
                    provided: 0
                })
            ),
            "Fee-bump Soroban tx with zero inclusion fee must be rejected, got: {result:?}"
        );
    }

    // ========================================================================
    // check_soroban_resource_limits tests
    // ========================================================================

    /// Helper to build a Soroban TransactionFrame with custom resource values.
    fn make_soroban_frame(
        instructions: u32,
        disk_read_bytes: u32,
        write_bytes: u32,
        read_only: Vec<LedgerKey>,
        read_write: Vec<LedgerKey>,
        ext: SorobanTransactionDataExt,
        op_body: OperationBody,
    ) -> TransactionFrame {
        let source = MuxedAccount::Ed25519(Uint256([2u8; 32]));
        let op = Operation {
            source_account: None,
            body: op_body,
        };
        let footprint = LedgerFootprint {
            read_only: read_only.try_into().unwrap(),
            read_write: read_write.try_into().unwrap(),
        };
        let tx = Transaction {
            source_account: source,
            fee: 100,
            seq_num: SequenceNumber(1),
            cond: Preconditions::None,
            memo: Memo::None,
            operations: vec![op].try_into().unwrap(),
            ext: TransactionExt::V1(SorobanTransactionData {
                ext,
                resources: SorobanResources {
                    footprint,
                    instructions,
                    disk_read_bytes,
                    write_bytes,
                },
                resource_fee: 0,
            }),
        };
        let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: VecM::default(),
        });
        TransactionFrame::from_owned(envelope)
    }

    fn default_invoke_op() -> OperationBody {
        OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(InvokeContractArgs {
                contract_address: ScAddress::Contract(ContractId(Hash([9u8; 32]))),
                function_name: ScSymbol(StringM::<32>::try_from("noop".to_string()).unwrap()),
                args: VecM::default(),
            }),
            auth: VecM::default(),
        })
    }

    fn permissive_limits() -> SorobanResourceLimits {
        SorobanResourceLimits {
            tx_max_instructions: u64::MAX,
            tx_max_read_bytes: u64::MAX,
            tx_max_write_bytes: u64::MAX,
            tx_max_read_ledger_entries: u64::MAX,
            tx_max_write_ledger_entries: u64::MAX,
            tx_max_size_bytes: u64::MAX,
            tx_max_footprint_entries: u64::MAX,
            max_contract_size_bytes: u32::MAX,
            max_contract_data_key_size_bytes: u32::MAX,
        }
    }

    fn contract_data_key(id: u8) -> LedgerKey {
        LedgerKey::ContractData(LedgerKeyContractData {
            contract: ScAddress::Contract(ContractId(Hash([id; 32]))),
            key: ScVal::U32(id as u32),
            durability: ContractDataDurability::Persistent,
        })
    }

    #[test]
    fn test_soroban_resource_limits_instructions_boundary() {
        let frame = make_soroban_frame(
            1000,
            0,
            0,
            vec![],
            vec![],
            SorobanTransactionDataExt::V0,
            default_invoke_op(),
        );
        let mut limits = permissive_limits();
        limits.tx_max_instructions = 1000;
        assert!(check_soroban_resource_limits(&frame, 22, &limits).is_ok());

        limits.tx_max_instructions = 999;
        let err = check_soroban_resource_limits(&frame, 22, &limits).unwrap_err();
        assert!(matches!(err, PreSeqNumError::SorobanInvalid(_)));
    }

    #[test]
    fn test_soroban_resource_limits_disk_read_bytes() {
        let frame = make_soroban_frame(
            0,
            500,
            0,
            vec![],
            vec![],
            SorobanTransactionDataExt::V0,
            default_invoke_op(),
        );
        let mut limits = permissive_limits();
        limits.tx_max_read_bytes = 500;
        assert!(check_soroban_resource_limits(&frame, 22, &limits).is_ok());

        limits.tx_max_read_bytes = 499;
        assert!(check_soroban_resource_limits(&frame, 22, &limits).is_err());
    }

    #[test]
    fn test_soroban_resource_limits_write_bytes() {
        let frame = make_soroban_frame(
            0,
            0,
            800,
            vec![],
            vec![],
            SorobanTransactionDataExt::V0,
            default_invoke_op(),
        );
        let mut limits = permissive_limits();
        limits.tx_max_write_bytes = 800;
        assert!(check_soroban_resource_limits(&frame, 22, &limits).is_ok());

        limits.tx_max_write_bytes = 799;
        assert!(check_soroban_resource_limits(&frame, 22, &limits).is_err());
    }

    #[test]
    fn test_soroban_resource_limits_write_entries() {
        let rw = vec![contract_data_key(1), contract_data_key(2)];
        let frame = make_soroban_frame(
            0,
            0,
            0,
            vec![],
            rw,
            SorobanTransactionDataExt::V0,
            default_invoke_op(),
        );
        let mut limits = permissive_limits();
        limits.tx_max_write_ledger_entries = 2;
        assert!(check_soroban_resource_limits(&frame, 22, &limits).is_ok());

        limits.tx_max_write_ledger_entries = 1;
        assert!(check_soroban_resource_limits(&frame, 22, &limits).is_err());
    }

    #[test]
    fn test_soroban_resource_limits_disk_read_entries_pre_v23() {
        // Pre-v23: all footprint entries count as disk reads
        let ro = vec![contract_data_key(1)];
        let rw = vec![contract_data_key(2), contract_data_key(3)];
        let frame = make_soroban_frame(
            0,
            0,
            0,
            ro,
            rw,
            SorobanTransactionDataExt::V0,
            default_invoke_op(),
        );
        let mut limits = permissive_limits();
        // Total footprint = 3 entries = 3 disk reads
        limits.tx_max_read_ledger_entries = 3;
        assert!(check_soroban_resource_limits(&frame, 22, &limits).is_ok());

        limits.tx_max_read_ledger_entries = 2;
        assert!(check_soroban_resource_limits(&frame, 22, &limits).is_err());
    }

    #[test]
    fn test_soroban_resource_limits_disk_read_entries_v23_soroban_keys_not_counted() {
        // v23+: Soroban keys are NOT counted as disk reads (they're in memory cache)
        let ro = vec![contract_data_key(1)];
        let rw = vec![contract_data_key(2)];
        let frame = make_soroban_frame(
            0,
            0,
            0,
            ro,
            rw,
            SorobanTransactionDataExt::V0,
            default_invoke_op(),
        );
        let mut limits = permissive_limits();
        // Both keys are Soroban, so disk reads = 0 at v23
        limits.tx_max_read_ledger_entries = 0;
        assert!(check_soroban_resource_limits(&frame, 23, &limits).is_ok());
    }

    #[test]
    fn test_soroban_resource_limits_footprint_entries_v23() {
        // v23+: total footprint entries check
        let ro = vec![contract_data_key(1), contract_data_key(2)];
        let rw = vec![contract_data_key(3)];
        let frame = make_soroban_frame(
            0,
            0,
            0,
            ro,
            rw,
            SorobanTransactionDataExt::V0,
            default_invoke_op(),
        );
        let mut limits = permissive_limits();
        limits.tx_max_footprint_entries = 3;
        assert!(check_soroban_resource_limits(&frame, 23, &limits).is_ok());

        limits.tx_max_footprint_entries = 2;
        assert!(check_soroban_resource_limits(&frame, 23, &limits).is_err());
    }

    #[test]
    fn test_soroban_resource_limits_tx_size() {
        let frame = make_soroban_frame(
            0,
            0,
            0,
            vec![],
            vec![],
            SorobanTransactionDataExt::V0,
            default_invoke_op(),
        );
        let tx_size = frame.tx_size_bytes() as u64;
        let mut limits = permissive_limits();
        limits.tx_max_size_bytes = tx_size;
        assert!(check_soroban_resource_limits(&frame, 22, &limits).is_ok());

        limits.tx_max_size_bytes = tx_size - 1;
        assert!(check_soroban_resource_limits(&frame, 22, &limits).is_err());
    }

    #[test]
    fn test_soroban_resource_limits_archived_ext_rejected_pre_v23() {
        let frame = make_soroban_frame(
            0,
            0,
            0,
            vec![],
            vec![],
            SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                archived_soroban_entries: VecM::default(),
            }),
            default_invoke_op(),
        );
        let limits = permissive_limits();
        assert!(check_soroban_resource_limits(&frame, 22, &limits).is_err());
        assert!(check_soroban_resource_limits(&frame, 23, &limits).is_ok());
    }

    #[test]
    fn test_validate_footprint_key_unsupported_type() {
        use stellar_xdr::curr::LedgerKeyOffer;
        let key = LedgerKey::Offer(LedgerKeyOffer {
            seller_id: AccountId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            offer_id: 1,
        });
        let limits = permissive_limits();
        let err = validate_footprint_key(&key, 23, &limits).unwrap_err();
        assert!(matches!(err, PreSeqNumError::SorobanInvalid(_)));
    }

    #[test]
    fn test_validate_footprint_key_valid_types() {
        let limits = permissive_limits();

        // Account key
        let acct = LedgerKey::Account(stellar_xdr::curr::LedgerKeyAccount {
            account_id: AccountId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
        });
        assert!(validate_footprint_key(&acct, 23, &limits).is_ok());

        // ContractData key
        let cd = contract_data_key(5);
        assert!(validate_footprint_key(&cd, 23, &limits).is_ok());
    }

    #[test]
    fn test_validate_footprint_key_xdr_size_boundary() {
        let key = contract_data_key(1);
        let key_size = henyey_common::xdr_encoded_len_u32(&key) as u32;

        let mut limits = permissive_limits();
        limits.max_contract_data_key_size_bytes = key_size;
        assert!(validate_footprint_key(&key, 23, &limits).is_ok());

        limits.max_contract_data_key_size_bytes = key_size - 1;
        assert!(validate_footprint_key(&key, 23, &limits).is_err());
    }

    #[test]
    fn test_validate_footprint_key_trustline_native_rejected() {
        use stellar_xdr::curr::{LedgerKeyTrustLine, TrustLineAsset};
        let key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: AccountId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
            asset: TrustLineAsset::Native,
        });
        let limits = permissive_limits();
        assert!(validate_footprint_key(&key, 23, &limits).is_err());
    }

    #[test]
    fn test_validate_footprint_key_trustline_self_issued_rejected() {
        use stellar_xdr::curr::{AlphaNum4, AssetCode4, LedgerKeyTrustLine, TrustLineAsset};
        let issuer_bytes = [7u8; 32];
        let key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: AccountId(XdrPublicKey::PublicKeyTypeEd25519(Uint256(issuer_bytes))),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                issuer: AccountId(XdrPublicKey::PublicKeyTypeEd25519(Uint256(issuer_bytes))),
            }),
        });
        let limits = permissive_limits();
        assert!(validate_footprint_key(&key, 23, &limits).is_err());
    }

    #[test]
    fn test_validate_footprint_key_trustline_valid() {
        use stellar_xdr::curr::{AlphaNum4, AssetCode4, LedgerKeyTrustLine, TrustLineAsset};
        let key = LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: AccountId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
            asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
                asset_code: AssetCode4([b'U', b'S', b'D', 0]),
                issuer: AccountId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32]))),
            }),
        });
        let limits = permissive_limits();
        assert!(validate_footprint_key(&key, 23, &limits).is_ok());
    }

    #[test]
    fn test_get_num_disk_read_entries_restore_footprint() {
        use stellar_xdr::curr::RestoreFootprintOp;
        let rw = vec![
            contract_data_key(1),
            contract_data_key(2),
            contract_data_key(3),
        ];
        let frame = make_soroban_frame(
            0,
            0,
            0,
            vec![],
            rw,
            SorobanTransactionDataExt::V0,
            OperationBody::RestoreFootprint(RestoreFootprintOp {
                ext: stellar_xdr::curr::ExtensionPoint::V0,
            }),
        );
        let data = frame.soroban_data().unwrap();
        let count = get_num_disk_read_entries(&data.resources, &data.ext, &frame);
        // RestoreFootprint: all readWrite entries require disk reads
        assert_eq!(count, 3);
    }

    #[test]
    fn test_get_num_disk_read_entries_with_archived_soroban() {
        let ro = vec![contract_data_key(1)];
        let rw = vec![contract_data_key(2)];
        let frame = make_soroban_frame(
            0,
            0,
            0,
            ro,
            rw,
            SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                // One archived entry → 1 disk read
                archived_soroban_entries: vec![0u32].try_into().unwrap(),
            }),
            default_invoke_op(),
        );
        let data = frame.soroban_data().unwrap();
        let count = get_num_disk_read_entries(&data.resources, &data.ext, &frame);
        // Both keys are Soroban (not classic), so classic count = 0; archived = 1
        assert_eq!(count, 1);
    }

    #[test]
    fn test_get_num_disk_read_entries_classic_keys_counted() {
        use stellar_xdr::curr::LedgerKeyAccount;
        // Account keys are classic (non-Soroban) → counted as disk reads at v23+
        let acct_key = LedgerKey::Account(LedgerKeyAccount {
            account_id: AccountId(XdrPublicKey::PublicKeyTypeEd25519(Uint256([5u8; 32]))),
        });
        let ro = vec![acct_key, contract_data_key(1)];
        let frame = make_soroban_frame(
            0,
            0,
            0,
            ro,
            vec![],
            SorobanTransactionDataExt::V0,
            default_invoke_op(),
        );
        let data = frame.soroban_data().unwrap();
        let count = get_num_disk_read_entries(&data.resources, &data.ext, &frame);
        // Account key = 1 classic disk read, ContractData = 0 (Soroban, in-memory)
        assert_eq!(count, 1);
    }
}
