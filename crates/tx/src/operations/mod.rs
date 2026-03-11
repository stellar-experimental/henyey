//! Operation types and validation.
//!
//! This module enumerates all Stellar operation types and provides
//! basic validation for each operation, as well as execution logic.

pub mod execute;
pub use execute::prefetch::collect_prefetch_keys;

use std::collections::HashSet;

use stellar_xdr::curr::{
    AllowTrustOp, BeginSponsoringFutureReservesOp, BumpSequenceOp, ChangeTrustOp,
    ClaimClaimableBalanceOp, ClaimPredicate, Claimant, ClawbackClaimableBalanceOp, ClawbackOp,
    CreateAccountOp, CreateClaimableBalanceOp, CreatePassiveSellOfferOp, ExtendFootprintTtlOp,
    InvokeHostFunctionOp, LiquidityPoolDepositOp, LiquidityPoolWithdrawOp, ManageBuyOfferOp,
    ManageDataOp, ManageSellOfferOp, MuxedAccount, Operation, OperationBody, OperationType,
    PathPaymentStrictReceiveOp, PathPaymentStrictSendOp, PaymentOp, RestoreFootprintOp,
    SetOptionsOp, SetTrustLineFlagsOp,
};

/// Extension trait for `stellar_xdr::curr::OperationType` providing Soroban classification
/// and body-to-type conversion.
pub trait OperationTypeExt {
    /// Check if this is a Soroban operation.
    fn is_soroban(&self) -> bool;

    /// Check if this is a classic operation.
    fn is_classic(&self) -> bool;

    /// Get the operation type from an operation body.
    fn from_body(body: &OperationBody) -> Self;
}

impl OperationTypeExt for OperationType {
    fn is_soroban(&self) -> bool {
        matches!(
            self,
            OperationType::InvokeHostFunction
                | OperationType::ExtendFootprintTtl
                | OperationType::RestoreFootprint
        )
    }

    fn is_classic(&self) -> bool {
        !self.is_soroban()
    }

    fn from_body(body: &OperationBody) -> Self {
        body.discriminant()
    }
}

/// Validation error for operations.
#[derive(Debug, Clone)]
pub enum OperationValidationError {
    /// Invalid amount (negative or zero when positive required).
    InvalidAmount(i64),
    /// Invalid destination.
    InvalidDestination,
    /// Invalid asset.
    InvalidAsset(String),
    /// Invalid data value.
    InvalidDataValue(String),
    /// Invalid threshold.
    InvalidThreshold,
    /// Invalid weight.
    InvalidWeight,
    /// Invalid offer ID.
    InvalidOfferId,
    /// Invalid price.
    InvalidPrice,
    /// Invalid claimant.
    InvalidClaimant,
    /// Invalid pool ID.
    InvalidPoolId,
    /// Invalid host function.
    InvalidHostFunction(String),
    /// Invalid Soroban data.
    InvalidSorobanData(String),
    /// Generic validation error.
    Other(String),
}

impl std::fmt::Display for OperationValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidAmount(amt) => write!(f, "invalid amount: {}", amt),
            Self::InvalidDestination => write!(f, "invalid destination"),
            Self::InvalidAsset(msg) => write!(f, "invalid asset: {}", msg),
            Self::InvalidDataValue(msg) => write!(f, "invalid data value: {}", msg),
            Self::InvalidThreshold => write!(f, "invalid threshold"),
            Self::InvalidWeight => write!(f, "invalid weight"),
            Self::InvalidOfferId => write!(f, "invalid offer ID"),
            Self::InvalidPrice => write!(f, "invalid price"),
            Self::InvalidClaimant => write!(f, "invalid claimant"),
            Self::InvalidPoolId => write!(f, "invalid pool ID"),
            Self::InvalidHostFunction(msg) => write!(f, "invalid host function: {}", msg),
            Self::InvalidSorobanData(msg) => write!(f, "invalid Soroban data: {}", msg),
            Self::Other(msg) => write!(f, "{}", msg),
        }
    }
}

/// Validate an operation.
pub fn validate_operation(op: &Operation) -> std::result::Result<(), OperationValidationError> {
    match &op.body {
        OperationBody::CreateAccount(op) => validate_create_account(op),
        OperationBody::Payment(op) => validate_payment(op),
        OperationBody::PathPaymentStrictReceive(op) => validate_path_payment_strict_receive(op),
        OperationBody::PathPaymentStrictSend(op) => validate_path_payment_strict_send(op),
        OperationBody::ManageSellOffer(op) => validate_manage_sell_offer(op),
        OperationBody::ManageBuyOffer(op) => validate_manage_buy_offer(op),
        OperationBody::CreatePassiveSellOffer(op) => validate_create_passive_sell_offer(op),
        OperationBody::SetOptions(op) => validate_set_options(op),
        OperationBody::ChangeTrust(op) => validate_change_trust(op),
        OperationBody::AllowTrust(op) => validate_allow_trust(op),
        OperationBody::AccountMerge(_) => Ok(()), // No specific validation needed
        OperationBody::Inflation => Ok(()),       // No validation needed
        OperationBody::ManageData(op) => validate_manage_data(op),
        OperationBody::BumpSequence(op) => validate_bump_sequence(op),
        OperationBody::CreateClaimableBalance(op) => validate_create_claimable_balance(op),
        OperationBody::ClaimClaimableBalance(op) => validate_claim_claimable_balance(op),
        OperationBody::BeginSponsoringFutureReserves(op) => {
            validate_begin_sponsoring_future_reserves(op)
        }
        OperationBody::EndSponsoringFutureReserves => Ok(()), // No validation needed
        OperationBody::RevokeSponsorship(_) => Ok(()),        // Complex, trust for now
        OperationBody::Clawback(op) => validate_clawback(op),
        OperationBody::ClawbackClaimableBalance(op) => validate_clawback_claimable_balance(op),
        OperationBody::SetTrustLineFlags(op) => validate_set_trust_line_flags(op),
        OperationBody::LiquidityPoolDeposit(op) => validate_liquidity_pool_deposit(op),
        OperationBody::LiquidityPoolWithdraw(op) => validate_liquidity_pool_withdraw(op),
        OperationBody::InvokeHostFunction(op) => validate_invoke_host_function(op),
        OperationBody::ExtendFootprintTtl(op) => validate_extend_footprint_ttl(op),
        OperationBody::RestoreFootprint(op) => validate_restore_footprint(op),
    }
}

/// Validate CreateAccount operation.
fn validate_create_account(
    op: &CreateAccountOp,
) -> std::result::Result<(), OperationValidationError> {
    // Spec: TX_SPEC §12.1 — @version(>=14): startingBalance MUST be >= 0.
    // Henyey only supports protocol 24+, so zero is allowed.
    if op.starting_balance < 0 {
        return Err(OperationValidationError::InvalidAmount(op.starting_balance));
    }
    Ok(())
}

/// Validate Payment operation.
fn validate_payment(op: &PaymentOp) -> std::result::Result<(), OperationValidationError> {
    if op.amount <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.amount));
    }
    Ok(())
}

/// Validate PathPaymentStrictReceive operation.
fn validate_path_payment_strict_receive(
    op: &PathPaymentStrictReceiveOp,
) -> std::result::Result<(), OperationValidationError> {
    if op.dest_amount <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.dest_amount));
    }
    if op.send_max <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.send_max));
    }
    Ok(())
}

/// Validate PathPaymentStrictSend operation.
fn validate_path_payment_strict_send(
    op: &PathPaymentStrictSendOp,
) -> std::result::Result<(), OperationValidationError> {
    if op.send_amount <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.send_amount));
    }
    if op.dest_min <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.dest_min));
    }
    Ok(())
}

/// Validate ManageSellOffer operation.
fn validate_manage_sell_offer(
    op: &ManageSellOfferOp,
) -> std::result::Result<(), OperationValidationError> {
    // Amount of 0 is valid (deletes offer)
    if op.amount < 0 {
        return Err(OperationValidationError::InvalidAmount(op.amount));
    }
    // Price must be positive
    if op.price.n <= 0 || op.price.d <= 0 {
        return Err(OperationValidationError::InvalidPrice);
    }
    Ok(())
}

/// Validate ManageBuyOffer operation.
fn validate_manage_buy_offer(
    op: &ManageBuyOfferOp,
) -> std::result::Result<(), OperationValidationError> {
    if op.buy_amount < 0 {
        return Err(OperationValidationError::InvalidAmount(op.buy_amount));
    }
    if op.price.n <= 0 || op.price.d <= 0 {
        return Err(OperationValidationError::InvalidPrice);
    }
    Ok(())
}

/// Validate CreatePassiveSellOffer operation.
fn validate_create_passive_sell_offer(
    op: &CreatePassiveSellOfferOp,
) -> std::result::Result<(), OperationValidationError> {
    if op.amount <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.amount));
    }
    if op.price.n <= 0 || op.price.d <= 0 {
        return Err(OperationValidationError::InvalidPrice);
    }
    Ok(())
}

/// Validate SetOptions operation.
fn validate_set_options(op: &SetOptionsOp) -> std::result::Result<(), OperationValidationError> {
    // Check master weight if set
    if let Some(weight) = op.master_weight {
        if weight > 255 {
            return Err(OperationValidationError::InvalidWeight);
        }
    }
    // Check thresholds if set
    if let Some(t) = op.low_threshold {
        if t > 255 {
            return Err(OperationValidationError::InvalidThreshold);
        }
    }
    if let Some(t) = op.med_threshold {
        if t > 255 {
            return Err(OperationValidationError::InvalidThreshold);
        }
    }
    if let Some(t) = op.high_threshold {
        if t > 255 {
            return Err(OperationValidationError::InvalidThreshold);
        }
    }
    Ok(())
}

/// Validate ChangeTrust operation.
fn validate_change_trust(op: &ChangeTrustOp) -> std::result::Result<(), OperationValidationError> {
    // Limit of 0 is valid (removes trustline)
    if op.limit < 0 {
        return Err(OperationValidationError::InvalidAmount(op.limit));
    }
    Ok(())
}

/// Validate AllowTrust operation.
fn validate_allow_trust(_op: &AllowTrustOp) -> std::result::Result<(), OperationValidationError> {
    // Basic structure is validated by XDR
    Ok(())
}

/// Validate ManageData operation.
fn validate_manage_data(op: &ManageDataOp) -> std::result::Result<(), OperationValidationError> {
    // Data name must not be empty
    if op.data_name.is_empty() {
        return Err(OperationValidationError::InvalidDataValue(
            "data name cannot be empty".to_string(),
        ));
    }
    // Data value (if present) must be <= 64 bytes
    if let Some(value) = &op.data_value {
        if value.len() > 64 {
            return Err(OperationValidationError::InvalidDataValue(
                "data value exceeds 64 bytes".to_string(),
            ));
        }
    }
    Ok(())
}

/// Validate BumpSequence operation.
fn validate_bump_sequence(
    op: &BumpSequenceOp,
) -> std::result::Result<(), OperationValidationError> {
    if op.bump_to.0 < 0 {
        return Err(OperationValidationError::InvalidAmount(op.bump_to.0));
    }
    Ok(())
}

/// Validate CreateClaimableBalance operation.
fn validate_create_claimable_balance(
    op: &CreateClaimableBalanceOp,
) -> std::result::Result<(), OperationValidationError> {
    if op.amount <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.amount));
    }
    if op.claimants.is_empty() {
        return Err(OperationValidationError::InvalidClaimant);
    }
    let mut destinations = HashSet::new();
    for claimant in op.claimants.iter() {
        let Claimant::ClaimantTypeV0(cv0) = claimant;
        if !destinations.insert(cv0.destination.clone()) {
            return Err(OperationValidationError::InvalidClaimant);
        }
        if !validate_claim_predicate(&cv0.predicate, 1) {
            return Err(OperationValidationError::InvalidClaimant);
        }
    }
    Ok(())
}

fn validate_claim_predicate(predicate: &ClaimPredicate, depth: u32) -> bool {
    if depth > 4 {
        return false;
    }
    match predicate {
        ClaimPredicate::Unconditional => true,
        ClaimPredicate::And(predicates) => {
            predicates.len() == 2
                && validate_claim_predicate(&predicates[0], depth + 1)
                && validate_claim_predicate(&predicates[1], depth + 1)
        }
        ClaimPredicate::Or(predicates) => {
            predicates.len() == 2
                && validate_claim_predicate(&predicates[0], depth + 1)
                && validate_claim_predicate(&predicates[1], depth + 1)
        }
        ClaimPredicate::Not(predicate) => predicate
            .as_ref()
            .map(|inner| validate_claim_predicate(inner, depth + 1))
            .unwrap_or(false),
        ClaimPredicate::BeforeAbsoluteTime(time) => *time >= 0,
        ClaimPredicate::BeforeRelativeTime(time) => *time >= 0,
    }
}

/// Validate ClaimClaimableBalance operation.
fn validate_claim_claimable_balance(
    _op: &ClaimClaimableBalanceOp,
) -> std::result::Result<(), OperationValidationError> {
    // Balance ID validation is handled by XDR
    Ok(())
}

/// Validate BeginSponsoringFutureReserves operation.
fn validate_begin_sponsoring_future_reserves(
    _op: &BeginSponsoringFutureReservesOp,
) -> std::result::Result<(), OperationValidationError> {
    // Account ID validation is handled by XDR
    Ok(())
}

/// Validate Clawback operation.
fn validate_clawback(op: &ClawbackOp) -> std::result::Result<(), OperationValidationError> {
    if op.amount <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.amount));
    }
    Ok(())
}

/// Validate ClawbackClaimableBalance operation.
fn validate_clawback_claimable_balance(
    _op: &ClawbackClaimableBalanceOp,
) -> std::result::Result<(), OperationValidationError> {
    // Balance ID validation is handled by XDR
    Ok(())
}

/// Validate SetTrustLineFlags operation.
fn validate_set_trust_line_flags(
    _op: &SetTrustLineFlagsOp,
) -> std::result::Result<(), OperationValidationError> {
    // Flags validation is handled by XDR
    Ok(())
}

/// Validate LiquidityPoolDeposit operation.
fn validate_liquidity_pool_deposit(
    op: &LiquidityPoolDepositOp,
) -> std::result::Result<(), OperationValidationError> {
    if op.max_amount_a <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.max_amount_a));
    }
    if op.max_amount_b <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.max_amount_b));
    }
    if op.min_price.n <= 0 || op.min_price.d <= 0 || op.max_price.n <= 0 || op.max_price.d <= 0 {
        return Err(OperationValidationError::InvalidPrice);
    }
    if (op.min_price.n as i128) * (op.max_price.d as i128)
        > (op.min_price.d as i128) * (op.max_price.n as i128)
    {
        return Err(OperationValidationError::InvalidPrice);
    }
    Ok(())
}

/// Validate LiquidityPoolWithdraw operation.
fn validate_liquidity_pool_withdraw(
    op: &LiquidityPoolWithdrawOp,
) -> std::result::Result<(), OperationValidationError> {
    if op.amount <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.amount));
    }
    if op.min_amount_a < 0 {
        return Err(OperationValidationError::InvalidAmount(op.min_amount_a));
    }
    if op.min_amount_b < 0 {
        return Err(OperationValidationError::InvalidAmount(op.min_amount_b));
    }
    Ok(())
}

/// Validate InvokeHostFunction operation.
fn validate_invoke_host_function(
    _op: &InvokeHostFunctionOp,
) -> std::result::Result<(), OperationValidationError> {
    // Soroban validation is complex, trust XDR for basic structure
    Ok(())
}

/// Validate ExtendFootprintTtl operation.
fn validate_extend_footprint_ttl(
    op: &ExtendFootprintTtlOp,
) -> std::result::Result<(), OperationValidationError> {
    if op.extend_to == 0 {
        return Err(OperationValidationError::InvalidSorobanData(
            "extend_to must be positive".to_string(),
        ));
    }
    Ok(())
}

/// Validate RestoreFootprint operation.
fn validate_restore_footprint(
    _op: &RestoreFootprintOp,
) -> std::result::Result<(), OperationValidationError> {
    // Structure validation is handled by XDR
    Ok(())
}

/// Get the source account for an operation.
///
/// If the operation has an explicit source, use that.
/// Otherwise, the transaction source is used.
pub fn get_operation_source<'a>(
    op: &'a Operation,
    tx_source: &'a MuxedAccount,
) -> &'a MuxedAccount {
    op.source_account.as_ref().unwrap_or(tx_source)
}

// Re-export ThresholdLevel from henyey_common so downstream users can still
// access it via `henyey_tx::ThresholdLevel`.
pub use henyey_common::ThresholdLevel;

/// Get the threshold level required for an operation.
///
/// This determines how much signer weight is needed to authorize the operation,
/// based on the stellar-core implementation.
///
/// # Threshold Assignments
///
/// - **Low**: Operations that don't significantly affect account security:
///   `AllowTrust`, `SetTrustLineFlags`, `BumpSequence`, `ClaimClaimableBalance`,
///   `Inflation`, `ExtendFootprintTtl`, `RestoreFootprint`
///
/// - **Medium**: Most standard operations including payments, offers, and data
///
/// - **High**: Operations that can affect account security:
///   `AccountMerge`, `SetOptions` (when modifying thresholds, weights, or signers)
pub fn get_threshold_level(op: &Operation) -> ThresholdLevel {
    match &op.body {
        // LOW threshold operations
        OperationBody::AllowTrust(_) => ThresholdLevel::Low,
        OperationBody::SetTrustLineFlags(_) => ThresholdLevel::Low,
        OperationBody::BumpSequence(_) => ThresholdLevel::Low,
        OperationBody::ClaimClaimableBalance(_) => ThresholdLevel::Low,
        OperationBody::Inflation => ThresholdLevel::Low,
        OperationBody::ExtendFootprintTtl(_) => ThresholdLevel::Low,
        OperationBody::RestoreFootprint(_) => ThresholdLevel::Low,

        // HIGH threshold operations
        OperationBody::AccountMerge(_) => ThresholdLevel::High,
        OperationBody::SetOptions(set_options) => {
            // SetOptions requires HIGH threshold when modifying thresholds or signers
            if set_options.master_weight.is_some()
                || set_options.low_threshold.is_some()
                || set_options.med_threshold.is_some()
                || set_options.high_threshold.is_some()
                || set_options.signer.is_some()
            {
                ThresholdLevel::High
            } else {
                ThresholdLevel::Medium
            }
        }

        // All other operations use MEDIUM threshold
        _ => ThresholdLevel::Medium,
    }
}

/// Get the needed weight for an operation from the source account.
///
/// Looks up the threshold value from the account's `thresholds` array
/// based on the threshold level required for the operation.
///
/// # Arguments
///
/// * `account` - The account entry containing threshold configuration
/// * `level` - The threshold level required
///
/// # Returns
///
/// The threshold value (0-255) as an i32.
pub fn get_needed_threshold(
    account: &stellar_xdr::curr::AccountEntry,
    level: ThresholdLevel,
) -> i32 {
    account.thresholds.0[level.index()] as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::curr::*;

    #[test]
    fn test_operation_type_from_body() {
        let payment = OperationBody::Payment(PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([0u8; 32])),
            asset: Asset::Native,
            amount: 1000,
        });

        let op_type = OperationType::from_body(&payment);
        assert_eq!(op_type, OperationType::Payment);
        assert!(!op_type.is_soroban());
        assert!(op_type.is_classic());
    }

    #[test]
    fn test_soroban_operation_types() {
        assert!(OperationType::InvokeHostFunction.is_soroban());
        assert!(OperationType::ExtendFootprintTtl.is_soroban());
        assert!(OperationType::RestoreFootprint.is_soroban());

        assert!(!OperationType::Payment.is_soroban());
        assert!(!OperationType::CreateAccount.is_soroban());
    }

    #[test]
    fn test_validate_payment() {
        let valid = PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([0u8; 32])),
            asset: Asset::Native,
            amount: 1000,
        };
        assert!(validate_payment(&valid).is_ok());

        let invalid = PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([0u8; 32])),
            asset: Asset::Native,
            amount: 0,
        };
        assert!(validate_payment(&invalid).is_err());

        let negative = PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([0u8; 32])),
            asset: Asset::Native,
            amount: -100,
        };
        assert!(validate_payment(&negative).is_err());
    }

    #[test]
    fn test_validate_create_account() {
        let valid = CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            starting_balance: 10_000_000,
        };
        assert!(validate_create_account(&valid).is_ok());

        // P14+ allows startingBalance == 0
        let zero = CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            starting_balance: 0,
        };
        assert!(validate_create_account(&zero).is_ok());

        // Negative startingBalance is always rejected
        let negative = CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            starting_balance: -1,
        };
        assert!(validate_create_account(&negative).is_err());
    }

    #[test]
    fn test_operation_type_name() {
        assert_eq!(OperationType::Payment.name(), "Payment");
        assert_eq!(
            OperationType::InvokeHostFunction.name(),
            "InvokeHostFunction"
        );
    }

    #[test]
    fn test_validate_liquidity_pool_deposit() {
        let valid = LiquidityPoolDepositOp {
            liquidity_pool_id: PoolId(Hash([0u8; 32])),
            max_amount_a: 100,
            max_amount_b: 200,
            min_price: Price { n: 1, d: 2 },
            max_price: Price { n: 2, d: 1 },
        };
        assert!(validate_liquidity_pool_deposit(&valid).is_ok());

        let invalid_amount = LiquidityPoolDepositOp {
            max_amount_a: 0,
            ..valid.clone()
        };
        assert!(validate_liquidity_pool_deposit(&invalid_amount).is_err());

        let invalid_price = LiquidityPoolDepositOp {
            min_price: Price { n: 2, d: 1 },
            max_price: Price { n: 1, d: 1 },
            ..valid
        };
        assert!(validate_liquidity_pool_deposit(&invalid_price).is_err());
    }

    #[test]
    fn test_validate_liquidity_pool_withdraw() {
        let valid = LiquidityPoolWithdrawOp {
            liquidity_pool_id: PoolId(Hash([0u8; 32])),
            amount: 100,
            min_amount_a: 0,
            min_amount_b: 0,
        };
        assert!(validate_liquidity_pool_withdraw(&valid).is_ok());

        let invalid_amount = LiquidityPoolWithdrawOp {
            amount: 0,
            ..valid.clone()
        };
        assert!(validate_liquidity_pool_withdraw(&invalid_amount).is_err());

        let invalid_min = LiquidityPoolWithdrawOp {
            amount: 100,
            min_amount_a: -1,
            min_amount_b: 0,
            ..valid
        };
        assert!(validate_liquidity_pool_withdraw(&invalid_min).is_err());
    }

    #[test]
    fn test_validate_create_claimable_balance_duplicate_claimants() {
        let claimant_id = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32])));
        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: claimant_id,
            predicate: ClaimPredicate::Unconditional,
        });

        let op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 100,
            claimants: vec![claimant.clone(), claimant].try_into().unwrap(),
        };

        assert!(validate_create_claimable_balance(&op).is_err());
    }

    #[test]
    fn test_validate_create_claimable_balance_invalid_predicate() {
        let claimant = Claimant::ClaimantTypeV0(ClaimantV0 {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
            predicate: ClaimPredicate::Not(None),
        });

        let op = CreateClaimableBalanceOp {
            asset: Asset::Native,
            amount: 100,
            claimants: vec![claimant].try_into().unwrap(),
        };

        assert!(validate_create_claimable_balance(&op).is_err());
    }

    #[test]
    fn test_threshold_level_index() {
        assert_eq!(ThresholdLevel::Low.index(), 1);
        assert_eq!(ThresholdLevel::Medium.index(), 2);
        assert_eq!(ThresholdLevel::High.index(), 3);
    }

    #[test]
    fn test_low_threshold_operations() {
        // AllowTrust
        let allow_trust_op = Operation {
            source_account: None,
            body: OperationBody::AllowTrust(AllowTrustOp {
                trustor: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
                asset: stellar_xdr::curr::AssetCode::CreditAlphanum4(
                    stellar_xdr::curr::AssetCode4([b'U', b'S', b'D', 0]),
                ),
                authorize: 1,
            }),
        };
        assert_eq!(get_threshold_level(&allow_trust_op), ThresholdLevel::Low);

        // BumpSequence
        let bump_seq_op = Operation {
            source_account: None,
            body: OperationBody::BumpSequence(BumpSequenceOp {
                bump_to: stellar_xdr::curr::SequenceNumber(100),
            }),
        };
        assert_eq!(get_threshold_level(&bump_seq_op), ThresholdLevel::Low);

        // ClaimClaimableBalance
        let claim_op = Operation {
            source_account: None,
            body: OperationBody::ClaimClaimableBalance(ClaimClaimableBalanceOp {
                balance_id: stellar_xdr::curr::ClaimableBalanceId::ClaimableBalanceIdTypeV0(
                    stellar_xdr::curr::Hash([0u8; 32]),
                ),
            }),
        };
        assert_eq!(get_threshold_level(&claim_op), ThresholdLevel::Low);

        // Inflation
        let inflation_op = Operation {
            source_account: None,
            body: OperationBody::Inflation,
        };
        assert_eq!(get_threshold_level(&inflation_op), ThresholdLevel::Low);
    }

    #[test]
    fn test_medium_threshold_operations() {
        // Payment
        let payment_op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: MuxedAccount::Ed25519(Uint256([0u8; 32])),
                asset: Asset::Native,
                amount: 1000,
            }),
        };
        assert_eq!(get_threshold_level(&payment_op), ThresholdLevel::Medium);

        // CreateAccount
        let create_account_op = Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
                starting_balance: 10_000_000,
            }),
        };
        assert_eq!(
            get_threshold_level(&create_account_op),
            ThresholdLevel::Medium
        );

        // ChangeTrust
        let change_trust_op = Operation {
            source_account: None,
            body: OperationBody::ChangeTrust(ChangeTrustOp {
                line: stellar_xdr::curr::ChangeTrustAsset::Native,
                limit: 1000,
            }),
        };
        assert_eq!(
            get_threshold_level(&change_trust_op),
            ThresholdLevel::Medium
        );

        // ManageData
        let manage_data_op = Operation {
            source_account: None,
            body: OperationBody::ManageData(ManageDataOp {
                data_name: stellar_xdr::curr::String64::try_from(b"test".to_vec()).unwrap(),
                data_value: Some(b"value".to_vec().try_into().unwrap()),
            }),
        };
        assert_eq!(get_threshold_level(&manage_data_op), ThresholdLevel::Medium);
    }

    #[test]
    fn test_high_threshold_operations() {
        // AccountMerge
        let account_merge_op = Operation {
            source_account: None,
            body: OperationBody::AccountMerge(MuxedAccount::Ed25519(Uint256([0u8; 32]))),
        };
        assert_eq!(get_threshold_level(&account_merge_op), ThresholdLevel::High);

        // SetOptions with threshold change
        let set_options_threshold_op = Operation {
            source_account: None,
            body: OperationBody::SetOptions(SetOptionsOp {
                inflation_dest: None,
                clear_flags: None,
                set_flags: None,
                master_weight: None,
                low_threshold: Some(10),
                med_threshold: None,
                high_threshold: None,
                home_domain: None,
                signer: None,
            }),
        };
        assert_eq!(
            get_threshold_level(&set_options_threshold_op),
            ThresholdLevel::High
        );

        // SetOptions with signer change
        let set_options_signer_op = Operation {
            source_account: None,
            body: OperationBody::SetOptions(SetOptionsOp {
                inflation_dest: None,
                clear_flags: None,
                set_flags: None,
                master_weight: None,
                low_threshold: None,
                med_threshold: None,
                high_threshold: None,
                home_domain: None,
                signer: Some(stellar_xdr::curr::Signer {
                    key: stellar_xdr::curr::SignerKey::Ed25519(Uint256([0u8; 32])),
                    weight: 10,
                }),
            }),
        };
        assert_eq!(
            get_threshold_level(&set_options_signer_op),
            ThresholdLevel::High
        );

        // SetOptions with master weight change
        let set_options_master_op = Operation {
            source_account: None,
            body: OperationBody::SetOptions(SetOptionsOp {
                inflation_dest: None,
                clear_flags: None,
                set_flags: None,
                master_weight: Some(5),
                low_threshold: None,
                med_threshold: None,
                high_threshold: None,
                home_domain: None,
                signer: None,
            }),
        };
        assert_eq!(
            get_threshold_level(&set_options_master_op),
            ThresholdLevel::High
        );
    }

    #[test]
    fn test_set_options_medium_threshold() {
        // SetOptions without security-related changes uses MEDIUM threshold
        let set_options_basic_op = Operation {
            source_account: None,
            body: OperationBody::SetOptions(SetOptionsOp {
                inflation_dest: Some(AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(
                    [0u8; 32],
                )))),
                clear_flags: None,
                set_flags: None,
                master_weight: None,
                low_threshold: None,
                med_threshold: None,
                high_threshold: None,
                home_domain: None,
                signer: None,
            }),
        };
        assert_eq!(
            get_threshold_level(&set_options_basic_op),
            ThresholdLevel::Medium
        );

        // SetOptions with only home domain change
        let set_options_domain_op = Operation {
            source_account: None,
            body: OperationBody::SetOptions(SetOptionsOp {
                inflation_dest: None,
                clear_flags: None,
                set_flags: None,
                master_weight: None,
                low_threshold: None,
                med_threshold: None,
                high_threshold: None,
                home_domain: Some(
                    stellar_xdr::curr::String32::try_from(b"example.com".to_vec()).unwrap(),
                ),
                signer: None,
            }),
        };
        assert_eq!(
            get_threshold_level(&set_options_domain_op),
            ThresholdLevel::Medium
        );
    }

    #[test]
    fn test_get_needed_threshold() {
        use stellar_xdr::curr::{
            AccountEntry, AccountEntryExt, SequenceNumber, String32, Thresholds, VecM,
        };

        let account = AccountEntry {
            account_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            balance: 1000,
            seq_num: SequenceNumber(1),
            num_sub_entries: 0,
            inflation_dest: None,
            flags: 0,
            home_domain: String32::default(),
            thresholds: Thresholds([10, 1, 5, 10]), // master=10, low=1, med=5, high=10
            signers: VecM::default(),
            ext: AccountEntryExt::V0,
        };

        assert_eq!(get_needed_threshold(&account, ThresholdLevel::Low), 1);
        assert_eq!(get_needed_threshold(&account, ThresholdLevel::Medium), 5);
        assert_eq!(get_needed_threshold(&account, ThresholdLevel::High), 10);
    }

    /// Test validate_path_payment_strict_receive.
    #[test]
    fn test_validate_path_payment_strict_receive() {
        let valid = PathPaymentStrictReceiveOp {
            send_asset: Asset::Native,
            send_max: 1000,
            destination: MuxedAccount::Ed25519(Uint256([0u8; 32])),
            dest_asset: Asset::Native,
            dest_amount: 500,
            path: vec![].try_into().unwrap(),
        };
        assert!(validate_path_payment_strict_receive(&valid).is_ok());

        // Zero send_max
        let invalid_send = PathPaymentStrictReceiveOp {
            send_max: 0,
            ..valid.clone()
        };
        assert!(validate_path_payment_strict_receive(&invalid_send).is_err());

        // Zero dest_amount
        let invalid_dest = PathPaymentStrictReceiveOp {
            dest_amount: 0,
            ..valid.clone()
        };
        assert!(validate_path_payment_strict_receive(&invalid_dest).is_err());

        // Negative send_max
        let negative_send = PathPaymentStrictReceiveOp {
            send_max: -100,
            ..valid.clone()
        };
        assert!(validate_path_payment_strict_receive(&negative_send).is_err());

        // Negative dest_amount
        let negative_dest = PathPaymentStrictReceiveOp {
            dest_amount: -100,
            ..valid
        };
        assert!(validate_path_payment_strict_receive(&negative_dest).is_err());
    }

    /// Test validate_path_payment_strict_send.
    #[test]
    fn test_validate_path_payment_strict_send() {
        let valid = PathPaymentStrictSendOp {
            send_asset: Asset::Native,
            send_amount: 1000,
            destination: MuxedAccount::Ed25519(Uint256([0u8; 32])),
            dest_asset: Asset::Native,
            dest_min: 500,
            path: vec![].try_into().unwrap(),
        };
        assert!(validate_path_payment_strict_send(&valid).is_ok());

        // Zero send_amount
        let invalid_send = PathPaymentStrictSendOp {
            send_amount: 0,
            ..valid.clone()
        };
        assert!(validate_path_payment_strict_send(&invalid_send).is_err());

        // Negative dest_min
        let negative_dest = PathPaymentStrictSendOp {
            dest_min: -100,
            ..valid
        };
        assert!(validate_path_payment_strict_send(&negative_dest).is_err());
    }

    /// Test validate_manage_sell_offer.
    #[test]
    fn test_validate_manage_sell_offer() {
        let valid = ManageSellOfferOp {
            selling: Asset::Native,
            buying: Asset::CreditAlphanum4(stellar_xdr::curr::AlphaNum4 {
                asset_code: stellar_xdr::curr::AssetCode4([b'U', b'S', b'D', 0]),
                issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            }),
            amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };
        assert!(validate_manage_sell_offer(&valid).is_ok());

        // Zero amount is valid (delete offer)
        let zero_amount = ManageSellOfferOp {
            amount: 0,
            ..valid.clone()
        };
        assert!(validate_manage_sell_offer(&zero_amount).is_ok());

        // Negative amount
        let negative = ManageSellOfferOp {
            amount: -100,
            ..valid.clone()
        };
        assert!(validate_manage_sell_offer(&negative).is_err());

        // Zero price numerator
        let zero_price_n = ManageSellOfferOp {
            price: Price { n: 0, d: 1 },
            ..valid.clone()
        };
        assert!(validate_manage_sell_offer(&zero_price_n).is_err());

        // Zero price denominator
        let zero_price_d = ManageSellOfferOp {
            price: Price { n: 1, d: 0 },
            ..valid.clone()
        };
        assert!(validate_manage_sell_offer(&zero_price_d).is_err());

        // Negative price
        let negative_price = ManageSellOfferOp {
            price: Price { n: -1, d: 1 },
            ..valid
        };
        assert!(validate_manage_sell_offer(&negative_price).is_err());
    }

    /// Test validate_manage_buy_offer.
    #[test]
    fn test_validate_manage_buy_offer() {
        let valid = ManageBuyOfferOp {
            selling: Asset::Native,
            buying: Asset::CreditAlphanum4(stellar_xdr::curr::AlphaNum4 {
                asset_code: stellar_xdr::curr::AssetCode4([b'U', b'S', b'D', 0]),
                issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            }),
            buy_amount: 1000,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        };
        assert!(validate_manage_buy_offer(&valid).is_ok());

        // Negative buy_amount
        let negative = ManageBuyOfferOp {
            buy_amount: -100,
            ..valid
        };
        assert!(validate_manage_buy_offer(&negative).is_err());
    }

    /// Test validate_set_options.
    #[test]
    fn test_validate_set_options() {
        let valid = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };
        assert!(validate_set_options(&valid).is_ok());

        // Valid master weight
        let valid_weight = SetOptionsOp {
            master_weight: Some(100),
            ..valid.clone()
        };
        assert!(validate_set_options(&valid_weight).is_ok());

        // Invalid master weight (> 255)
        let invalid_weight = SetOptionsOp {
            master_weight: Some(256),
            ..valid.clone()
        };
        assert!(validate_set_options(&invalid_weight).is_err());

        // Invalid low threshold (> 255)
        let invalid_low = SetOptionsOp {
            low_threshold: Some(256),
            ..valid.clone()
        };
        assert!(validate_set_options(&invalid_low).is_err());

        // Invalid med threshold (> 255)
        let invalid_med = SetOptionsOp {
            med_threshold: Some(256),
            ..valid.clone()
        };
        assert!(validate_set_options(&invalid_med).is_err());

        // Invalid high threshold (> 255)
        let invalid_high = SetOptionsOp {
            high_threshold: Some(256),
            ..valid
        };
        assert!(validate_set_options(&invalid_high).is_err());
    }

    /// Test validate_change_trust.
    #[test]
    fn test_validate_change_trust() {
        use stellar_xdr::curr::ChangeTrustAsset;

        let valid = ChangeTrustOp {
            line: ChangeTrustAsset::CreditAlphanum4(stellar_xdr::curr::AlphaNum4 {
                asset_code: stellar_xdr::curr::AssetCode4([b'U', b'S', b'D', 0]),
                issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            }),
            limit: 1000,
        };
        assert!(validate_change_trust(&valid).is_ok());

        // Negative limit
        let negative = ChangeTrustOp {
            limit: -100,
            ..valid
        };
        assert!(validate_change_trust(&negative).is_err());
    }

    /// Test validate_bump_sequence.
    #[test]
    fn test_validate_bump_sequence() {
        let valid = BumpSequenceOp {
            bump_to: stellar_xdr::curr::SequenceNumber(100),
        };
        assert!(validate_bump_sequence(&valid).is_ok());

        // Negative bump_to
        let negative = BumpSequenceOp {
            bump_to: stellar_xdr::curr::SequenceNumber(-1),
        };
        assert!(validate_bump_sequence(&negative).is_err());
    }

    /// Test validate_manage_data.
    #[test]
    fn test_validate_manage_data() {
        let valid = ManageDataOp {
            data_name: stellar_xdr::curr::String64::try_from(b"test".to_vec()).unwrap(),
            data_value: Some(b"value".to_vec().try_into().unwrap()),
        };
        assert!(validate_manage_data(&valid).is_ok());

        // Delete operation (None value) is also valid
        let delete = ManageDataOp {
            data_name: stellar_xdr::curr::String64::try_from(b"test".to_vec()).unwrap(),
            data_value: None,
        };
        assert!(validate_manage_data(&delete).is_ok());
    }

    /// Test all operation types have names.
    #[test]
    fn test_all_operation_type_names() {
        // Classic operations
        assert!(!OperationType::CreateAccount.name().is_empty());
        assert!(!OperationType::Payment.name().is_empty());
        assert!(!OperationType::PathPaymentStrictReceive.name().is_empty());
        assert!(!OperationType::ManageSellOffer.name().is_empty());
        assert!(!OperationType::CreatePassiveSellOffer.name().is_empty());
        assert!(!OperationType::SetOptions.name().is_empty());
        assert!(!OperationType::ChangeTrust.name().is_empty());
        assert!(!OperationType::AllowTrust.name().is_empty());
        assert!(!OperationType::AccountMerge.name().is_empty());
        assert!(!OperationType::Inflation.name().is_empty());
        assert!(!OperationType::ManageData.name().is_empty());
        assert!(!OperationType::BumpSequence.name().is_empty());
        assert!(!OperationType::ManageBuyOffer.name().is_empty());
        assert!(!OperationType::PathPaymentStrictSend.name().is_empty());
        assert!(!OperationType::CreateClaimableBalance.name().is_empty());
        assert!(!OperationType::ClaimClaimableBalance.name().is_empty());
        assert!(!OperationType::BeginSponsoringFutureReserves
            .name()
            .is_empty());
        assert!(!OperationType::EndSponsoringFutureReserves.name().is_empty());
        assert!(!OperationType::RevokeSponsorship.name().is_empty());
        assert!(!OperationType::Clawback.name().is_empty());
        assert!(!OperationType::ClawbackClaimableBalance.name().is_empty());
        assert!(!OperationType::SetTrustLineFlags.name().is_empty());
        assert!(!OperationType::LiquidityPoolDeposit.name().is_empty());
        assert!(!OperationType::LiquidityPoolWithdraw.name().is_empty());

        // Soroban operations
        assert!(!OperationType::InvokeHostFunction.name().is_empty());
        assert!(!OperationType::ExtendFootprintTtl.name().is_empty());
        assert!(!OperationType::RestoreFootprint.name().is_empty());
    }

    /// Test OperationType from_body for all operation types.
    #[test]
    fn test_operation_type_from_body_all() {
        // Test CreateAccount
        let create_account = OperationBody::CreateAccount(CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            starting_balance: 10_000_000,
        });
        assert_eq!(
            OperationType::from_body(&create_account),
            OperationType::CreateAccount
        );

        // Test ManageSellOffer
        let manage_sell = OperationBody::ManageSellOffer(ManageSellOfferOp {
            selling: Asset::Native,
            buying: Asset::Native,
            amount: 100,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        });
        assert_eq!(
            OperationType::from_body(&manage_sell),
            OperationType::ManageSellOffer
        );

        // Test ManageBuyOffer
        let manage_buy = OperationBody::ManageBuyOffer(ManageBuyOfferOp {
            selling: Asset::Native,
            buying: Asset::Native,
            buy_amount: 100,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        });
        assert_eq!(
            OperationType::from_body(&manage_buy),
            OperationType::ManageBuyOffer
        );

        // Test Inflation
        let inflation = OperationBody::Inflation;
        assert_eq!(
            OperationType::from_body(&inflation),
            OperationType::Inflation
        );
    }

    /// Test OperationValidationError display.
    #[test]
    fn test_operation_validation_error_display() {
        let err = OperationValidationError::InvalidAmount(-100);
        let display = err.to_string();
        assert!(display.contains("-100") || display.contains("amount"));

        let err = OperationValidationError::InvalidAsset("same asset".to_string());
        assert!(err.to_string().contains("same asset"));

        let err = OperationValidationError::InvalidPrice;
        let display = err.to_string();
        assert!(display.contains("price") || display.contains("Price"));

        let err = OperationValidationError::InvalidDestination;
        assert!(!err.to_string().is_empty());
    }
}
