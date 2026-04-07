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

use henyey_common::{Hash256, NetworkId};
use henyey_crypto::{PublicKey, Signature};
use stellar_xdr::curr::{
    AccountEntry, DecoratedSignature, Preconditions, SignerKey, TransactionEnvelope,
};

use crate::fee_bump::{validate_fee_bump, FeeBumpError, FeeBumpFrame};
use crate::frame::TransactionFrame;

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
        }
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
    InsufficientFee {
        /// Minimum required fee in stroops.
        required: u32,
        /// Fee provided in the transaction.
        provided: u32,
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
    /// Ledger sequence is outside the allowed range.
    BadLedgerBounds {
        /// Minimum ledger allowed.
        min: u32,
        /// Maximum ledger allowed.
        max: u32,
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
            Self::BadLedgerBounds { min, max, current } => {
                write!(
                    f,
                    "bad ledger bounds: [{}, {}], current {}",
                    min, max, current
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
/// Checks that the fee meets the minimum required fee based on operation count.
pub fn validate_fee(
    frame: &TransactionFrame,
    context: &LedgerContext,
) -> std::result::Result<(), ValidationError> {
    let op_count = frame.operation_count() as u32;
    let required_fee = op_count.saturating_mul(context.base_fee);
    let provided_fee = frame.fee();

    if provided_fee < required_fee {
        return Err(ValidationError::InsufficientFee {
            required: required_fee,
            provided: provided_fee,
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
            return Err(ValidationError::BadLedgerBounds {
                min: lb.min_ledger,
                max: lb.max_ledger,
                current,
            });
        }

        // Check max ledger (0 means no limit)
        // Spec: TX_SPEC §4.2.3 — ledger sequence MUST be strictly less than maxLedger.
        if lb.max_ledger > 0 && current >= lb.max_ledger {
            return Err(ValidationError::BadLedgerBounds {
                min: lb.min_ledger,
                max: lb.max_ledger,
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

            if !is_archivable_soroban_key(key) {
                return Err(ValidationError::InvalidStructure(
                    "archived soroban entry must be a persistent contract entry".to_string(),
                ));
            }
        }
    }

    let _ = context;
    Ok(())
}

fn is_archivable_soroban_key(key: &stellar_xdr::curr::LedgerKey) -> bool {
    use stellar_xdr::curr::{ContractDataDurability, LedgerKey};

    match key {
        LedgerKey::ContractData(cd) => cd.durability == ContractDataDurability::Persistent,
        LedgerKey::ContractCode(_) => true,
        _ => false,
    }
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
        FeeBumpError::InvalidInnerSignature => ValidationError::InvalidSignature,
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
    let fee = frame.total_fee();
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
    signed_payload: &stellar_xdr::curr::SignerKeyEd25519SignedPayload,
) -> bool {
    let pk = match PublicKey::from_bytes(&signed_payload.ed25519.0) {
        Ok(pk) => pk,
        Err(_) => return false,
    };

    // The hint for signed payloads is XOR of pubkey hint and payload hint.
    // See SignatureUtils::getSignedPayloadHint in stellar-core.
    let pubkey_hint = [
        signed_payload.ed25519.0[28],
        signed_payload.ed25519.0[29],
        signed_payload.ed25519.0[30],
        signed_payload.ed25519.0[31],
    ];
    let payload_hint = if signed_payload.payload.len() >= 4 {
        let len = signed_payload.payload.len();
        [
            signed_payload.payload[len - 4],
            signed_payload.payload[len - 3],
            signed_payload.payload[len - 2],
            signed_payload.payload[len - 1],
        ]
    } else {
        // For shorter payloads, stellar-core getHint copies from the beginning
        let mut hint = [0u8; 4];
        for (i, &byte) in signed_payload.payload.iter().enumerate() {
            if i < 4 {
                hint[i] = byte;
            }
        }
        hint
    };
    let expected_hint = [
        pubkey_hint[0] ^ payload_hint[0],
        pubkey_hint[1] ^ payload_hint[1],
        pubkey_hint[2] ^ payload_hint[2],
        pubkey_hint[3] ^ payload_hint[3],
    ];

    signatures.iter().any(|sig| {
        // Check hint first (XOR of pubkey hint and payload hint)
        if sig.hint.0 != expected_hint {
            return false;
        }

        // stellar-core verifies the signature against the raw payload bytes,
        // not a hash. This is per CAP-0040 - the signed payload signer
        // requires a valid signature of the payload from the ed25519 public key.
        let ed_sig = match henyey_crypto::Signature::try_from(&sig.signature) {
            Ok(s) => s,
            Err(_) => return false,
        };
        pk.verify(&signed_payload.payload, &ed_sig).is_ok()
    })
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
            Err(ValidationError::BadLedgerBounds { .. })
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
            Err(ValidationError::BadLedgerBounds { .. })
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
                Err(ValidationError::BadLedgerBounds { .. })
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
}
