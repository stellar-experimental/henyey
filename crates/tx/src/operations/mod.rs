//! Operation types and validation.
//!
//! This module enumerates all Stellar operation types and provides
//! basic validation for each operation, as well as execution logic.

pub mod execute;
pub use execute::prefetch::collect_prefetch_keys;

use std::collections::HashSet;

use crate::frame::muxed_to_account_id;
use henyey_common::asset::{
    is_asset_valid, is_change_trust_asset_valid, is_string_valid, is_trustline_asset_valid,
};
use stellar_xdr::curr::{
    AccountId, AllowTrustOp, Asset, BeginSponsoringFutureReservesOp, BumpSequenceOp,
    ChangeTrustAsset, ChangeTrustOp, ClaimClaimableBalanceOp, ClaimPredicate, Claimant,
    ClawbackClaimableBalanceOp, ClawbackOp, CreateAccountOp, CreateClaimableBalanceOp,
    CreatePassiveSellOfferOp, ExtendFootprintTtlOp, InvokeHostFunctionOp, LedgerHeaderFlags,
    LedgerKey, LiquidityPoolDepositOp, LiquidityPoolWithdrawOp, ManageBuyOfferOp, ManageDataOp,
    ManageSellOfferOp, MuxedAccount, Operation, OperationBody, OperationType,
    PathPaymentStrictReceiveOp, PathPaymentStrictSendOp, PaymentOp, RestoreFootprintOp,
    RevokeSponsorshipOp, SetOptionsOp, SetTrustLineFlagsOp, SignerKey, TrustLineFlags,
    MASK_ACCOUNT_FLAGS_V17,
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
    /// Invalid signer (self-signer, weight out of range, invalid payload).
    /// Used by SetOptions; maps to SET_OPTIONS_BAD_SIGNER.
    InvalidSigner,
    /// Invalid home domain string (non-printable/control chars or invalid UTF-8).
    /// Used by SetOptions; maps to SET_OPTIONS_INVALID_HOME_DOMAIN.
    InvalidHomeDomain,
    /// Set and clear flags have overlapping bits.
    /// Used by SetOptions; maps to SET_OPTIONS_BAD_FLAGS.
    FlagOverlap,
    /// Flag value contains unknown/unsupported bits outside the valid mask.
    /// Used by SetOptions; maps to SET_OPTIONS_UNKNOWN_FLAG.
    UnknownFlags,
    /// Operation type not supported at the current protocol version or ledger flags.
    NotSupported(OperationType),
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
            Self::InvalidSigner => write!(f, "invalid signer"),
            Self::InvalidHomeDomain => write!(f, "invalid home domain"),
            Self::FlagOverlap => write!(f, "set and clear flags overlap"),
            Self::UnknownFlags => write!(f, "unknown flags"),
            Self::NotSupported(op) => write!(f, "operation not supported: {:?}", op),
            Self::Other(msg) => write!(f, "{}", msg),
        }
    }
}

/// Check whether an operation type is supported at the given protocol version
/// and ledger header flags.
///
/// Mirrors stellar-core's `OperationFrame::isOpSupported(LedgerHeader)`.
pub fn is_op_supported(
    op_type: &OperationType,
    _protocol_version: u32,
    ledger_flags: u32,
) -> std::result::Result<(), OperationValidationError> {
    match op_type {
        // Inflation removed in protocol 12; Henyey minimum is p24.
        OperationType::Inflation => Err(OperationValidationError::NotSupported(*op_type)),
        // LP deposit/withdraw gated by ledger header flags.
        OperationType::LiquidityPoolDeposit => {
            if ledger_flags & (LedgerHeaderFlags::DepositFlag as u32) != 0 {
                Err(OperationValidationError::NotSupported(*op_type))
            } else {
                Ok(())
            }
        }
        OperationType::LiquidityPoolWithdraw => {
            if ledger_flags & (LedgerHeaderFlags::WithdrawalFlag as u32) != 0 {
                Err(OperationValidationError::NotSupported(*op_type))
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

/// Validate an operation.
///
/// Mirrors stellar-core's `OperationFrame::checkValid()`: first checks
/// `isOpSupported` (protocol version + ledger flags), then dispatches to
/// per-operation structural validation (`doCheckValid`).
///
/// `source_account` is the effective source for this operation (op-level source
/// if set, otherwise the transaction source). Some checks (e.g. destination !=
/// source, issuer == source) require it. When `None`, those checks are skipped.
pub fn validate_operation(
    op: &Operation,
    protocol_version: u32,
    ledger_flags: u32,
    source_account: Option<&AccountId>,
) -> std::result::Result<(), OperationValidationError> {
    // Phase 1: Protocol/flag gating (isOpSupported)
    let op_type = OperationType::from_body(&op.body);
    is_op_supported(&op_type, protocol_version, ledger_flags)?;

    // Phase 2: Per-op structural checks (doCheckValid)
    match &op.body {
        OperationBody::CreateAccount(inner) => validate_create_account(inner, source_account),
        OperationBody::Payment(inner) => validate_payment(inner, protocol_version),
        OperationBody::PathPaymentStrictReceive(inner) => {
            validate_path_payment_strict_receive(inner, protocol_version)
        }
        OperationBody::PathPaymentStrictSend(inner) => {
            validate_path_payment_strict_send(inner, protocol_version)
        }
        OperationBody::ManageSellOffer(inner) => {
            validate_manage_sell_offer(inner, protocol_version)
        }
        OperationBody::ManageBuyOffer(inner) => validate_manage_buy_offer(inner, protocol_version),
        OperationBody::CreatePassiveSellOffer(inner) => {
            validate_create_passive_sell_offer(inner, protocol_version)
        }
        OperationBody::SetOptions(inner) => {
            validate_set_options(inner, protocol_version, source_account)
        }
        OperationBody::ChangeTrust(inner) => {
            validate_change_trust(inner, protocol_version, source_account)
        }
        OperationBody::AllowTrust(inner) => {
            validate_allow_trust(inner, protocol_version, source_account)
        }
        OperationBody::AccountMerge(dest) => validate_account_merge(dest, source_account),
        OperationBody::Inflation => Ok(()), // No validation needed
        OperationBody::ManageData(inner) => validate_manage_data(inner, protocol_version),
        OperationBody::BumpSequence(inner) => validate_bump_sequence(inner),
        OperationBody::CreateClaimableBalance(inner) => {
            validate_create_claimable_balance(inner, protocol_version)
        }
        OperationBody::ClaimClaimableBalance(inner) => validate_claim_claimable_balance(inner),
        OperationBody::BeginSponsoringFutureReserves(inner) => {
            validate_begin_sponsoring_future_reserves(inner, source_account)
        }
        OperationBody::EndSponsoringFutureReserves => Ok(()),
        OperationBody::RevokeSponsorship(inner) => {
            validate_revoke_sponsorship(inner, protocol_version)
        }
        OperationBody::Clawback(inner) => {
            validate_clawback(inner, protocol_version, source_account)
        }
        OperationBody::ClawbackClaimableBalance(inner) => {
            validate_clawback_claimable_balance(inner)
        }
        OperationBody::SetTrustLineFlags(inner) => {
            validate_set_trust_line_flags(inner, protocol_version, source_account)
        }
        OperationBody::LiquidityPoolDeposit(inner) => validate_liquidity_pool_deposit(inner),
        OperationBody::LiquidityPoolWithdraw(inner) => validate_liquidity_pool_withdraw(inner),
        OperationBody::InvokeHostFunction(inner) => {
            validate_invoke_host_function(inner, protocol_version)
        }
        OperationBody::ExtendFootprintTtl(inner) => validate_extend_footprint_ttl(inner),
        OperationBody::RestoreFootprint(inner) => validate_restore_footprint(inner),
    }
}

/// Validate CreateAccount operation.
fn validate_create_account(
    op: &CreateAccountOp,
    source_account: Option<&AccountId>,
) -> std::result::Result<(), OperationValidationError> {
    // Henyey only supports protocol 24+, so zero starting balance is allowed.
    if op.starting_balance < 0 {
        return Err(OperationValidationError::InvalidAmount(op.starting_balance));
    }
    // destination must not be source (stellar-core CreateAccountOpFrame.cpp:187)
    if let Some(src) = source_account {
        if &op.destination == src {
            return Err(OperationValidationError::InvalidDestination);
        }
    }
    Ok(())
}

/// Validate Payment operation.
fn validate_payment(
    op: &PaymentOp,
    protocol_version: u32,
) -> std::result::Result<(), OperationValidationError> {
    if op.amount <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.amount));
    }
    if !is_asset_valid(&op.asset, protocol_version) {
        return Err(OperationValidationError::InvalidAsset(
            "invalid asset".into(),
        ));
    }
    Ok(())
}

/// Validate PathPaymentStrictReceive operation.
fn validate_path_payment_strict_receive(
    op: &PathPaymentStrictReceiveOp,
    protocol_version: u32,
) -> std::result::Result<(), OperationValidationError> {
    if op.dest_amount <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.dest_amount));
    }
    if op.send_max <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.send_max));
    }
    if !is_asset_valid(&op.send_asset, protocol_version)
        || !is_asset_valid(&op.dest_asset, protocol_version)
    {
        return Err(OperationValidationError::InvalidAsset(
            "invalid asset".into(),
        ));
    }
    for p in op.path.iter() {
        if !is_asset_valid(p, protocol_version) {
            return Err(OperationValidationError::InvalidAsset(
                "invalid asset in path".into(),
            ));
        }
    }
    Ok(())
}

/// Validate PathPaymentStrictSend operation.
fn validate_path_payment_strict_send(
    op: &PathPaymentStrictSendOp,
    protocol_version: u32,
) -> std::result::Result<(), OperationValidationError> {
    if op.send_amount <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.send_amount));
    }
    if op.dest_min <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.dest_min));
    }
    if !is_asset_valid(&op.send_asset, protocol_version)
        || !is_asset_valid(&op.dest_asset, protocol_version)
    {
        return Err(OperationValidationError::InvalidAsset(
            "invalid asset".into(),
        ));
    }
    for p in op.path.iter() {
        if !is_asset_valid(p, protocol_version) {
            return Err(OperationValidationError::InvalidAsset(
                "invalid asset in path".into(),
            ));
        }
    }
    Ok(())
}

/// Validate ManageSellOffer operation.
/// Mirrors stellar-core ManageOfferOpFrameBase::doCheckValid.
fn validate_manage_sell_offer(
    op: &ManageSellOfferOp,
    protocol_version: u32,
) -> std::result::Result<(), OperationValidationError> {
    if !is_asset_valid(&op.selling, protocol_version)
        || !is_asset_valid(&op.buying, protocol_version)
    {
        return Err(OperationValidationError::InvalidAsset(
            "invalid asset".into(),
        ));
    }
    if op.selling == op.buying {
        return Err(OperationValidationError::InvalidAsset(
            "selling and buying assets must differ".into(),
        ));
    }
    // Amount of 0 is valid (deletes offer)
    if op.amount < 0 {
        return Err(OperationValidationError::InvalidAmount(op.amount));
    }
    if op.price.n <= 0 || op.price.d <= 0 {
        return Err(OperationValidationError::InvalidPrice);
    }
    // p11+: creating an offer (id==0) with amount==0 is malformed
    if op.offer_id == 0 && op.amount == 0 {
        return Err(OperationValidationError::InvalidAmount(op.amount));
    }
    // p15+: negative offer IDs are invalid
    if op.offer_id < 0 {
        return Err(OperationValidationError::InvalidOfferId);
    }
    Ok(())
}

/// Validate ManageBuyOffer operation.
/// Validate ManageBuyOffer operation.
/// Same checks as ManageSellOffer (shared base in stellar-core).
fn validate_manage_buy_offer(
    op: &ManageBuyOfferOp,
    protocol_version: u32,
) -> std::result::Result<(), OperationValidationError> {
    if !is_asset_valid(&op.selling, protocol_version)
        || !is_asset_valid(&op.buying, protocol_version)
    {
        return Err(OperationValidationError::InvalidAsset(
            "invalid asset".into(),
        ));
    }
    if op.selling == op.buying {
        return Err(OperationValidationError::InvalidAsset(
            "selling and buying assets must differ".into(),
        ));
    }
    if op.buy_amount < 0 {
        return Err(OperationValidationError::InvalidAmount(op.buy_amount));
    }
    if op.price.n <= 0 || op.price.d <= 0 {
        return Err(OperationValidationError::InvalidPrice);
    }
    // p11+: creating an offer (id==0) with amount==0 is malformed
    if op.offer_id == 0 && op.buy_amount == 0 {
        return Err(OperationValidationError::InvalidAmount(op.buy_amount));
    }
    // p15+: negative offer IDs are invalid
    if op.offer_id < 0 {
        return Err(OperationValidationError::InvalidOfferId);
    }
    Ok(())
}

/// Validate CreatePassiveSellOffer operation.
/// Uses the same base validation as ManageSellOffer but amount must be > 0 and
/// there's no offer ID (passive offers are always new).
fn validate_create_passive_sell_offer(
    op: &CreatePassiveSellOfferOp,
    protocol_version: u32,
) -> std::result::Result<(), OperationValidationError> {
    if !is_asset_valid(&op.selling, protocol_version)
        || !is_asset_valid(&op.buying, protocol_version)
    {
        return Err(OperationValidationError::InvalidAsset(
            "invalid asset".into(),
        ));
    }
    if op.selling == op.buying {
        return Err(OperationValidationError::InvalidAsset(
            "selling and buying assets must differ".into(),
        ));
    }
    if op.amount <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.amount));
    }
    if op.price.n <= 0 || op.price.d <= 0 {
        return Err(OperationValidationError::InvalidPrice);
    }
    Ok(())
}

// SECURITY: signer weight bounded by XDR u32 type; protocol 8-bit limit enforced during operation execution in set_options.rs
/// Validate SetOptions operation.
/// Mirrors stellar-core SetOptionsOpFrame::doCheckValid.
fn validate_set_options(
    op: &SetOptionsOp,
    _protocol_version: u32,
    source_account: Option<&AccountId>,
) -> std::result::Result<(), OperationValidationError> {
    // Flag mask validation (p24+ uses V17 masks)
    let mask = MASK_ACCOUNT_FLAGS_V17 as u32;
    if let Some(flags) = op.set_flags {
        if flags & !mask != 0 {
            return Err(OperationValidationError::UnknownFlags);
        }
    }
    if let Some(flags) = op.clear_flags {
        if flags & !mask != 0 {
            return Err(OperationValidationError::UnknownFlags);
        }
    }
    // setFlags and clearFlags must not overlap
    if let (Some(set), Some(clear)) = (op.set_flags, op.clear_flags) {
        if (set & clear) != 0 {
            return Err(OperationValidationError::FlagOverlap);
        }
    }

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

    // Signer validation
    if let Some(signer) = &op.signer {
        // signer key must not be self
        if let Some(src) = source_account {
            if let SignerKey::Ed25519(key) = &signer.key {
                let signer_acct = AccountId(stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(
                    key.clone(),
                ));
                if &signer_acct == src {
                    return Err(OperationValidationError::InvalidSigner);
                }
            }
        }
        // p10+: signer weight must fit in u8
        if signer.weight > 255 {
            return Err(OperationValidationError::InvalidSigner);
        }
        // p19+: ED25519_SIGNED_PAYLOAD must have non-empty payload
        if let SignerKey::Ed25519SignedPayload(sp) = &signer.key {
            if sp.payload.is_empty() {
                return Err(OperationValidationError::InvalidSigner);
            }
        }
    }

    // Home domain string validation
    if let Some(domain) = &op.home_domain {
        let bytes: &[u8] = domain.as_ref();
        if !is_string_valid(std::str::from_utf8(bytes).unwrap_or("\0")) {
            return Err(OperationValidationError::InvalidHomeDomain);
        }
    }

    Ok(())
}

/// Validate ChangeTrust operation.
/// Mirrors stellar-core ChangeTrustOpFrame::doCheckValid.
fn validate_change_trust(
    op: &ChangeTrustOp,
    protocol_version: u32,
    source_account: Option<&AccountId>,
) -> std::result::Result<(), OperationValidationError> {
    if op.limit < 0 {
        return Err(OperationValidationError::InvalidAmount(op.limit));
    }
    if !is_change_trust_asset_valid(&op.line, protocol_version) {
        return Err(OperationValidationError::InvalidAsset(
            "invalid asset".into(),
        ));
    }
    // p10+: cannot create trustline for native asset
    if matches!(op.line, ChangeTrustAsset::Native) {
        return Err(OperationValidationError::InvalidAsset(
            "cannot create trustline for native asset".into(),
        ));
    }
    // p16+: source must not be issuer of the asset
    if let Some(src) = source_account {
        if is_change_trust_asset_issuer(src, &op.line) {
            return Err(OperationValidationError::InvalidAsset(
                "source is issuer of asset".into(),
            ));
        }
    }
    Ok(())
}

/// Check if an account is the issuer of a ChangeTrustAsset.
/// Pool shares have no issuer, so this returns false for them.
fn is_change_trust_asset_issuer(acc: &AccountId, asset: &ChangeTrustAsset) -> bool {
    match asset {
        ChangeTrustAsset::CreditAlphanum4(a) => acc == &a.issuer,
        ChangeTrustAsset::CreditAlphanum12(a) => acc == &a.issuer,
        ChangeTrustAsset::Native | ChangeTrustAsset::PoolShare(_) => false,
    }
}

/// Validate AllowTrust operation.
/// Mirrors stellar-core AllowTrustOpFrame::doCheckValid.
fn validate_allow_trust(
    op: &AllowTrustOp,
    _protocol_version: u32,
    source_account: Option<&AccountId>,
) -> std::result::Result<(), OperationValidationError> {
    // Asset must be a valid credit code. Mirrors stellar-core
    // AllowTrustOpFrame::doCheckValid() which calls isAssetValid().
    match &op.asset {
        stellar_xdr::curr::AssetCode::CreditAlphanum4(code) => {
            if !henyey_common::asset::is_asset_code4_valid(code) {
                return Err(OperationValidationError::InvalidAsset(
                    "invalid asset code".into(),
                ));
            }
        }
        stellar_xdr::curr::AssetCode::CreditAlphanum12(code) => {
            if !henyey_common::asset::is_asset_code12_valid(code) {
                return Err(OperationValidationError::InvalidAsset(
                    "invalid asset code".into(),
                ));
            }
        }
    }
    // authorize must be a valid trust line flag combination
    // AUTHORIZED_FLAG=1, AUTHORIZED_TO_MAINTAIN_LIABILITIES_FLAG=2
    // Valid values: 0, 1, 2 — NOT 3 (both auth flags)
    let auth = op.authorize;
    if auth > (TrustLineFlags::AuthorizedToMaintainLiabilitiesFlag as u32) {
        return Err(OperationValidationError::Other(
            "invalid authorize value".into(),
        ));
    }
    // Both auth flags set simultaneously is invalid (p13+)
    let both_auth = (TrustLineFlags::AuthorizedFlag as u32)
        | (TrustLineFlags::AuthorizedToMaintainLiabilitiesFlag as u32);
    if (auth & both_auth) == both_auth {
        return Err(OperationValidationError::Other(
            "cannot set both auth flags".into(),
        ));
    }
    // p16+: trustor must not be source
    if let Some(src) = source_account {
        if &op.trustor == src {
            return Err(OperationValidationError::Other(
                "trustor cannot be source".into(),
            ));
        }
    }
    Ok(())
}

/// Validate AccountMerge operation.
/// Mirrors stellar-core MergeOpFrame::doCheckValid.
fn validate_account_merge(
    dest: &MuxedAccount,
    source_account: Option<&AccountId>,
) -> std::result::Result<(), OperationValidationError> {
    // Destination must not be the same as source (ACCOUNT_MERGE_MALFORMED)
    if let Some(src) = source_account {
        let dest_id = muxed_to_account_id(dest);
        if *src == dest_id {
            return Err(OperationValidationError::InvalidDestination);
        }
    }
    Ok(())
}

/// Validate ManageData operation.
/// Mirrors stellar-core ManageDataOpFrame::doCheckValid.
fn validate_manage_data(
    op: &ManageDataOp,
    _protocol_version: u32,
) -> std::result::Result<(), OperationValidationError> {
    // Data name must not be empty and must be a valid string
    if op.data_name.is_empty()
        || !is_string_valid(std::str::from_utf8(op.data_name.as_ref()).unwrap_or("\0"))
    {
        return Err(OperationValidationError::InvalidDataValue(
            "data name is empty or contains invalid characters".to_string(),
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
    protocol_version: u32,
) -> std::result::Result<(), OperationValidationError> {
    if !is_asset_valid(&op.asset, protocol_version) {
        return Err(OperationValidationError::InvalidAsset(
            "invalid asset".into(),
        ));
    }
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
/// Mirrors stellar-core BeginSponsoringFutureReservesOpFrame::doCheckValid.
fn validate_begin_sponsoring_future_reserves(
    op: &BeginSponsoringFutureReservesOp,
    source_account: Option<&AccountId>,
) -> std::result::Result<(), OperationValidationError> {
    // Sponsored account must not be the source (BEGIN_SPONSORING_FUTURE_RESERVES_MALFORMED)
    if let Some(src) = source_account {
        if *src == op.sponsored_id {
            return Err(OperationValidationError::InvalidDestination);
        }
    }
    Ok(())
}

/// Validate RevokeSponsorship operation.
/// Mirrors stellar-core RevokeSponsorshipOpFrame::doCheckValid.
fn validate_revoke_sponsorship(
    op: &RevokeSponsorshipOp,
    protocol_version: u32,
) -> std::result::Result<(), OperationValidationError> {
    if let RevokeSponsorshipOp::LedgerEntry(lk) = op {
        match lk {
            LedgerKey::Account(_) | LedgerKey::ClaimableBalance(_) => {
                // No specific validation
            }
            LedgerKey::Trustline(tl) => {
                if !is_trustline_asset_valid(&tl.asset, protocol_version) {
                    return Err(OperationValidationError::InvalidAsset(
                        "invalid trustline asset".into(),
                    ));
                }
                if matches!(tl.asset, stellar_xdr::curr::TrustLineAsset::Native) {
                    return Err(OperationValidationError::InvalidAsset(
                        "trustline asset cannot be native".into(),
                    ));
                }
                if henyey_common::asset::is_trustline_asset_issuer(&tl.account_id, &tl.asset) {
                    return Err(OperationValidationError::Other(
                        "trustline account is issuer".into(),
                    ));
                }
            }
            LedgerKey::Offer(offer) => {
                if offer.offer_id <= 0 {
                    return Err(OperationValidationError::InvalidOfferId);
                }
            }
            LedgerKey::Data(data) => {
                let name_bytes: &[u8] = data.data_name.as_ref();
                if name_bytes.is_empty()
                    || !is_string_valid(std::str::from_utf8(name_bytes).unwrap_or(""))
                {
                    return Err(OperationValidationError::InvalidDataValue(
                        "invalid data name".into(),
                    ));
                }
            }
            // Unsupported ledger key types
            LedgerKey::LiquidityPool(_)
            | LedgerKey::ContractData(_)
            | LedgerKey::ContractCode(_)
            | LedgerKey::ConfigSetting(_)
            | LedgerKey::Ttl(_) => {
                return Err(OperationValidationError::Other(
                    "unsupported ledger key type for revoke sponsorship".into(),
                ));
            }
        }
    }
    // REVOKE_SPONSORSHIP_SIGNER has no additional validation in stellar-core
    Ok(())
}
/// Mirrors stellar-core ClawbackOpFrame::doCheckValid.
fn validate_clawback(
    op: &ClawbackOp,
    protocol_version: u32,
    source_account: Option<&AccountId>,
) -> std::result::Result<(), OperationValidationError> {
    // from must not be source — compare full MuxedAccount, not stripped AccountId.
    // stellar-core: mClawback.from == toMuxedAccount(getSourceID())
    // toMuxedAccount wraps AccountId as Ed25519, so a MuxedEd25519 `from`
    // with the same underlying key is considered different (clawback is allowed).
    if let Some(src) = source_account {
        let key = match &src.0 {
            stellar_xdr::curr::PublicKey::PublicKeyTypeEd25519(k) => k.clone(),
        };
        let src_muxed = MuxedAccount::Ed25519(key);
        if op.from == src_muxed {
            return Err(OperationValidationError::Other(
                "cannot clawback from self".into(),
            ));
        }
    }
    if op.amount <= 0 {
        return Err(OperationValidationError::InvalidAmount(op.amount));
    }
    // asset must not be native
    if matches!(op.asset, Asset::Native) {
        return Err(OperationValidationError::InvalidAsset(
            "cannot clawback native asset".into(),
        ));
    }
    if !is_asset_valid(&op.asset, protocol_version) {
        return Err(OperationValidationError::InvalidAsset(
            "invalid asset".into(),
        ));
    }
    // source must be issuer of the asset
    if let Some(src) = source_account {
        if !henyey_common::asset::is_issuer(src, &op.asset) {
            return Err(OperationValidationError::Other(
                "source must be asset issuer for clawback".into(),
            ));
        }
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
/// Mirrors stellar-core SetTrustLineFlagsOpFrame::doCheckValid.
fn validate_set_trust_line_flags(
    op: &SetTrustLineFlagsOp,
    protocol_version: u32,
    source_account: Option<&AccountId>,
) -> std::result::Result<(), OperationValidationError> {
    // asset must not be native
    if matches!(op.asset, Asset::Native) {
        return Err(OperationValidationError::InvalidAsset(
            "cannot set trust line flags for native asset".into(),
        ));
    }
    if !is_asset_valid(&op.asset, protocol_version) {
        return Err(OperationValidationError::InvalidAsset(
            "invalid asset".into(),
        ));
    }
    // source must be issuer
    if let Some(src) = source_account {
        if !henyey_common::asset::is_issuer(src, &op.asset) {
            return Err(OperationValidationError::Other(
                "source must be asset issuer".into(),
            ));
        }
        // trustor must not be source
        if &op.trustor == src {
            return Err(OperationValidationError::Other(
                "trustor cannot be source".into(),
            ));
        }
    }
    // setFlags and clearFlags must not overlap
    if (op.set_flags & op.clear_flags) != 0 {
        return Err(OperationValidationError::Other(
            "set and clear flags overlap".into(),
        ));
    }
    // setFlags must be valid trust line flags, and must not include clawback
    let set = op.set_flags;
    let both_auth = (TrustLineFlags::AuthorizedFlag as u32)
        | (TrustLineFlags::AuthorizedToMaintainLiabilitiesFlag as u32);
    if (set & both_auth) == both_auth {
        return Err(OperationValidationError::Other(
            "invalid set flags: both auth flags".into(),
        ));
    }
    if (set & (TrustLineFlags::TrustlineClawbackEnabledFlag as u32)) != 0 {
        return Err(OperationValidationError::Other(
            "cannot set clawback flag via SetTrustLineFlags".into(),
        ));
    }
    // For p24+, valid trust line flag mask = AuthorizedFlag | AuthorizedToMaintainLiabilitiesFlag | TrustlineClawbackEnabledFlag
    let valid_mask = (TrustLineFlags::AuthorizedFlag as u32)
        | (TrustLineFlags::AuthorizedToMaintainLiabilitiesFlag as u32)
        | (TrustLineFlags::TrustlineClawbackEnabledFlag as u32);
    if (set & !valid_mask) != 0 {
        return Err(OperationValidationError::Other("unknown set flags".into()));
    }
    // clearFlags mask check (same valid mask)
    if (op.clear_flags & !valid_mask) != 0 {
        return Err(OperationValidationError::Other(
            "unknown clear flags".into(),
        ));
    }
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
/// stellar-core: InvokeHostFunctionOpFrame::doCheckValidForSoroban (lines 1283-1313)
/// Note: Soroban-specific checks (WASM size, CreateContract asset) live in
/// check_valid_pre_seq_num section 4e so they return SorobanInvalid, not Malformed.
fn validate_invoke_host_function(
    _op: &InvokeHostFunctionOp,
    _protocol_version: u32,
) -> std::result::Result<(), OperationValidationError> {
    Ok(())
}

/// Validate ExtendFootprintTtl operation.
fn validate_extend_footprint_ttl(
    _op: &ExtendFootprintTtlOp,
) -> std::result::Result<(), OperationValidationError> {
    // stellar-core does not reject extend_to=0 in doCheckValid;
    // extend_to=0 is a valid no-op (target TTL <= any live entry's TTL).
    Ok(())
}

/// Validate RestoreFootprint operation.
fn validate_restore_footprint(
    _op: &RestoreFootprintOp,
) -> std::result::Result<(), OperationValidationError> {
    // Structure validation is handled by XDR
    Ok(())
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
                ThresholdLevel::Med
            }
        }

        // All other operations use MEDIUM threshold
        _ => ThresholdLevel::Med,
    }
}

/// Validate classic operation structural checks only (doCheckValid equivalent).
///
/// Runs per-operation structural validation WITHOUT the `is_op_supported()` check.
/// This mirrors stellar-core's `OperationFrame::doCheckValid()` which is called
/// after `isOpSupported()` and `checkSignature()` have already passed.
///
/// For Soroban operations this is a no-op — their structural validation is handled
/// by `doCheckValidForSoroban` in the pre-seq-num path.
pub fn validate_classic_op_structure(
    op: &Operation,
    protocol_version: u32,
    source_account: Option<&AccountId>,
) -> std::result::Result<(), OperationValidationError> {
    match &op.body {
        OperationBody::CreateAccount(inner) => validate_create_account(inner, source_account),
        OperationBody::Payment(inner) => validate_payment(inner, protocol_version),
        OperationBody::PathPaymentStrictReceive(inner) => {
            validate_path_payment_strict_receive(inner, protocol_version)
        }
        OperationBody::PathPaymentStrictSend(inner) => {
            validate_path_payment_strict_send(inner, protocol_version)
        }
        OperationBody::ManageSellOffer(inner) => {
            validate_manage_sell_offer(inner, protocol_version)
        }
        OperationBody::ManageBuyOffer(inner) => validate_manage_buy_offer(inner, protocol_version),
        OperationBody::CreatePassiveSellOffer(inner) => {
            validate_create_passive_sell_offer(inner, protocol_version)
        }
        OperationBody::SetOptions(inner) => {
            validate_set_options(inner, protocol_version, source_account)
        }
        OperationBody::ChangeTrust(inner) => {
            validate_change_trust(inner, protocol_version, source_account)
        }
        OperationBody::AllowTrust(inner) => {
            validate_allow_trust(inner, protocol_version, source_account)
        }
        OperationBody::AccountMerge(dest) => validate_account_merge(dest, source_account),
        OperationBody::Inflation => Ok(()), // Caught by is_op_supported, not doCheckValid
        OperationBody::ManageData(inner) => validate_manage_data(inner, protocol_version),
        OperationBody::BumpSequence(inner) => validate_bump_sequence(inner),
        OperationBody::CreateClaimableBalance(inner) => {
            validate_create_claimable_balance(inner, protocol_version)
        }
        OperationBody::ClaimClaimableBalance(inner) => validate_claim_claimable_balance(inner),
        OperationBody::BeginSponsoringFutureReserves(inner) => {
            validate_begin_sponsoring_future_reserves(inner, source_account)
        }
        OperationBody::EndSponsoringFutureReserves => Ok(()),
        OperationBody::RevokeSponsorship(inner) => {
            validate_revoke_sponsorship(inner, protocol_version)
        }
        OperationBody::Clawback(inner) => {
            validate_clawback(inner, protocol_version, source_account)
        }
        OperationBody::ClawbackClaimableBalance(inner) => {
            validate_clawback_claimable_balance(inner)
        }
        OperationBody::SetTrustLineFlags(inner) => {
            validate_set_trust_line_flags(inner, protocol_version, source_account)
        }
        OperationBody::LiquidityPoolDeposit(inner) => validate_liquidity_pool_deposit(inner),
        OperationBody::LiquidityPoolWithdraw(inner) => validate_liquidity_pool_withdraw(inner),
        // Soroban ops: structural validation handled in pre-seq-num path
        OperationBody::InvokeHostFunction(_)
        | OperationBody::ExtendFootprintTtl(_)
        | OperationBody::RestoreFootprint(_) => Ok(()),
    }
}

/// Map an operation body and validation error to the XDR `OperationResult` that
/// stellar-core's `doCheckValid()` would produce on failure.
///
/// Most operations use `*Result::Malformed`. Some operations use error-specific
/// codes (e.g., `SetOptionsResult::UnknownFlag`, `ManageDataResult::InvalidName`).
pub fn malformed_operation_result(
    body: &OperationBody,
    err: &OperationValidationError,
) -> stellar_xdr::curr::OperationResult {
    use stellar_xdr::curr::*;
    let tr = match body {
        OperationBody::CreateAccount(_) => {
            OperationResultTr::CreateAccount(CreateAccountResult::Malformed)
        }
        OperationBody::Payment(_) => OperationResultTr::Payment(PaymentResult::Malformed),
        OperationBody::PathPaymentStrictReceive(_) => {
            OperationResultTr::PathPaymentStrictReceive(PathPaymentStrictReceiveResult::Malformed)
        }
        OperationBody::PathPaymentStrictSend(_) => {
            OperationResultTr::PathPaymentStrictSend(PathPaymentStrictSendResult::Malformed)
        }
        OperationBody::ManageSellOffer(_) => {
            OperationResultTr::ManageSellOffer(ManageSellOfferResult::Malformed)
        }
        OperationBody::ManageBuyOffer(_) => {
            OperationResultTr::ManageBuyOffer(ManageBuyOfferResult::Malformed)
        }
        // CreatePassiveSellOffer uses ManageSellOffer result discriminant
        // (stellar-core: CreatePassiveSellOfferOpFrame inherits ManageSellOfferOpHolder)
        OperationBody::CreatePassiveSellOffer(_) => {
            OperationResultTr::ManageSellOffer(ManageSellOfferResult::Malformed)
        }
        // SetOptions: each validation error maps to a specific result code per
        // stellar-core SetOptionsOpFrame::doCheckValid(). All SetOptions validation
        // paths use typed variants at p24+; the catch-all should be unreachable.
        OperationBody::SetOptions(_) => {
            let code = match err {
                OperationValidationError::InvalidWeight
                | OperationValidationError::InvalidThreshold => {
                    SetOptionsResult::ThresholdOutOfRange
                }
                OperationValidationError::InvalidSigner => SetOptionsResult::BadSigner,
                OperationValidationError::InvalidHomeDomain => SetOptionsResult::InvalidHomeDomain,
                OperationValidationError::FlagOverlap => SetOptionsResult::BadFlags,
                OperationValidationError::UnknownFlags => SetOptionsResult::UnknownFlag,
                _ => SetOptionsResult::BadFlags,
            };
            OperationResultTr::SetOptions(code)
        }
        OperationBody::ChangeTrust(_) => {
            OperationResultTr::ChangeTrust(ChangeTrustResult::Malformed)
        }
        OperationBody::AllowTrust(_) => OperationResultTr::AllowTrust(AllowTrustResult::Malformed),
        OperationBody::AccountMerge(_) => {
            OperationResultTr::AccountMerge(AccountMergeResult::Malformed)
        }
        OperationBody::Inflation => OperationResultTr::Inflation(InflationResult::NotTime),
        // ManageData: stellar-core uses INVALID_NAME for doCheckValid failures at p24+.
        OperationBody::ManageData(_) => {
            OperationResultTr::ManageData(ManageDataResult::InvalidName)
        }
        // BumpSequence: stellar-core uses BAD_SEQ for negative bump_to.
        OperationBody::BumpSequence(_) => {
            OperationResultTr::BumpSequence(BumpSequenceResult::BadSeq)
        }
        OperationBody::CreateClaimableBalance(_) => {
            OperationResultTr::CreateClaimableBalance(CreateClaimableBalanceResult::Malformed)
        }
        OperationBody::ClaimClaimableBalance(_) => {
            OperationResultTr::ClaimClaimableBalance(ClaimClaimableBalanceResult::DoesNotExist)
        }
        OperationBody::BeginSponsoringFutureReserves(_) => {
            OperationResultTr::BeginSponsoringFutureReserves(
                BeginSponsoringFutureReservesResult::Malformed,
            )
        }
        OperationBody::EndSponsoringFutureReserves => {
            OperationResultTr::EndSponsoringFutureReserves(
                EndSponsoringFutureReservesResult::NotSponsored,
            )
        }
        // RevokeSponsorship: stellar-core's doCheckValid always returns
        // REVOKE_SPONSORSHIP_MALFORMED for structural validation failures
        // (RevokeSponsorshipOpFrame.cpp:420,428,437,449).
        OperationBody::RevokeSponsorship(_) => {
            OperationResultTr::RevokeSponsorship(RevokeSponsorshipResult::Malformed)
        }
        OperationBody::Clawback(_) => OperationResultTr::Clawback(ClawbackResult::Malformed),
        OperationBody::ClawbackClaimableBalance(_) => OperationResultTr::ClawbackClaimableBalance(
            ClawbackClaimableBalanceResult::DoesNotExist,
        ),
        OperationBody::SetTrustLineFlags(_) => {
            OperationResultTr::SetTrustLineFlags(SetTrustLineFlagsResult::Malformed)
        }
        OperationBody::LiquidityPoolDeposit(_) => {
            OperationResultTr::LiquidityPoolDeposit(LiquidityPoolDepositResult::Malformed)
        }
        OperationBody::LiquidityPoolWithdraw(_) => {
            OperationResultTr::LiquidityPoolWithdraw(LiquidityPoolWithdrawResult::Malformed)
        }
        // Soroban ops: structural validation handled in pre-seq-num path.
        OperationBody::InvokeHostFunction(_) => {
            OperationResultTr::InvokeHostFunction(InvokeHostFunctionResult::Malformed)
        }
        OperationBody::ExtendFootprintTtl(_) => {
            OperationResultTr::ExtendFootprintTtl(ExtendFootprintTtlResult::Malformed)
        }
        OperationBody::RestoreFootprint(_) => {
            OperationResultTr::RestoreFootprint(RestoreFootprintResult::Malformed)
        }
    };
    OperationResult::OpInner(tr)
}

#[cfg(test)]
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
    account.thresholds.0[level as usize] as i32
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
        assert!(validate_payment(&valid, 24).is_ok());

        let invalid = PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([0u8; 32])),
            asset: Asset::Native,
            amount: 0,
        };
        assert!(validate_payment(&invalid, 24).is_err());

        let negative = PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([0u8; 32])),
            asset: Asset::Native,
            amount: -100,
        };
        assert!(validate_payment(&negative, 24).is_err());
    }

    #[test]
    fn test_validate_create_account() {
        let valid = CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            starting_balance: 10_000_000,
        };
        assert!(validate_create_account(&valid, None).is_ok());

        // P14+ allows startingBalance == 0
        let zero = CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            starting_balance: 0,
        };
        assert!(validate_create_account(&zero, None).is_ok());

        // Negative startingBalance is always rejected
        let negative = CreateAccountOp {
            destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
            starting_balance: -1,
        };
        assert!(validate_create_account(&negative, None).is_err());
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

        assert!(validate_create_claimable_balance(&op, 24).is_err());
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

        assert!(validate_create_claimable_balance(&op, 24).is_err());
    }

    #[test]
    fn test_threshold_level_index() {
        assert_eq!(ThresholdLevel::Low as usize, 1);
        assert_eq!(ThresholdLevel::Med as usize, 2);
        assert_eq!(ThresholdLevel::High as usize, 3);
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
        assert_eq!(get_threshold_level(&payment_op), ThresholdLevel::Med);

        // CreateAccount
        let create_account_op = Operation {
            source_account: None,
            body: OperationBody::CreateAccount(CreateAccountOp {
                destination: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0u8; 32]))),
                starting_balance: 10_000_000,
            }),
        };
        assert_eq!(get_threshold_level(&create_account_op), ThresholdLevel::Med);

        // ChangeTrust
        let change_trust_op = Operation {
            source_account: None,
            body: OperationBody::ChangeTrust(ChangeTrustOp {
                line: stellar_xdr::curr::ChangeTrustAsset::Native,
                limit: 1000,
            }),
        };
        assert_eq!(get_threshold_level(&change_trust_op), ThresholdLevel::Med);

        // ManageData
        let manage_data_op = Operation {
            source_account: None,
            body: OperationBody::ManageData(ManageDataOp {
                data_name: stellar_xdr::curr::String64::try_from(b"test".to_vec()).unwrap(),
                data_value: Some(b"value".to_vec().try_into().unwrap()),
            }),
        };
        assert_eq!(get_threshold_level(&manage_data_op), ThresholdLevel::Med);
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
            ThresholdLevel::Med
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
            ThresholdLevel::Med
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
        assert_eq!(get_needed_threshold(&account, ThresholdLevel::Med), 5);
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
        assert!(validate_path_payment_strict_receive(&valid, 24).is_ok());

        // Zero send_max
        let invalid_send = PathPaymentStrictReceiveOp {
            send_max: 0,
            ..valid.clone()
        };
        assert!(validate_path_payment_strict_receive(&invalid_send, 24).is_err());

        // Zero dest_amount
        let invalid_dest = PathPaymentStrictReceiveOp {
            dest_amount: 0,
            ..valid.clone()
        };
        assert!(validate_path_payment_strict_receive(&invalid_dest, 24).is_err());

        // Negative send_max
        let negative_send = PathPaymentStrictReceiveOp {
            send_max: -100,
            ..valid.clone()
        };
        assert!(validate_path_payment_strict_receive(&negative_send, 24).is_err());

        // Negative dest_amount
        let negative_dest = PathPaymentStrictReceiveOp {
            dest_amount: -100,
            ..valid
        };
        assert!(validate_path_payment_strict_receive(&negative_dest, 24).is_err());
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
        assert!(validate_path_payment_strict_send(&valid, 24).is_ok());

        // Zero send_amount
        let invalid_send = PathPaymentStrictSendOp {
            send_amount: 0,
            ..valid.clone()
        };
        assert!(validate_path_payment_strict_send(&invalid_send, 24).is_err());

        // Negative dest_min
        let negative_dest = PathPaymentStrictSendOp {
            dest_min: -100,
            ..valid
        };
        assert!(validate_path_payment_strict_send(&negative_dest, 24).is_err());
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
        assert!(validate_manage_sell_offer(&valid, 24).is_ok());

        // Zero amount with existing offer_id is valid (delete offer)
        let zero_amount = ManageSellOfferOp {
            amount: 0,
            offer_id: 1,
            ..valid.clone()
        };
        assert!(validate_manage_sell_offer(&zero_amount, 24).is_ok());

        // Zero amount with offer_id == 0 is malformed (p11+: cannot create empty offer)
        let zero_create = ManageSellOfferOp {
            amount: 0,
            offer_id: 0,
            ..valid.clone()
        };
        assert!(validate_manage_sell_offer(&zero_create, 24).is_err());

        // Negative amount
        let negative = ManageSellOfferOp {
            amount: -100,
            ..valid.clone()
        };
        assert!(validate_manage_sell_offer(&negative, 24).is_err());

        // Zero price numerator
        let zero_price_n = ManageSellOfferOp {
            price: Price { n: 0, d: 1 },
            ..valid.clone()
        };
        assert!(validate_manage_sell_offer(&zero_price_n, 24).is_err());

        // Zero price denominator
        let zero_price_d = ManageSellOfferOp {
            price: Price { n: 1, d: 0 },
            ..valid.clone()
        };
        assert!(validate_manage_sell_offer(&zero_price_d, 24).is_err());

        // Negative price
        let negative_price = ManageSellOfferOp {
            price: Price { n: -1, d: 1 },
            ..valid
        };
        assert!(validate_manage_sell_offer(&negative_price, 24).is_err());
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
        assert!(validate_manage_buy_offer(&valid, 24).is_ok());

        // Negative buy_amount
        let negative = ManageBuyOfferOp {
            buy_amount: -100,
            ..valid
        };
        assert!(validate_manage_buy_offer(&negative, 24).is_err());
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
        assert!(validate_set_options(&valid, 24, None).is_ok());

        // Valid master weight
        let valid_weight = SetOptionsOp {
            master_weight: Some(100),
            ..valid.clone()
        };
        assert!(validate_set_options(&valid_weight, 24, None).is_ok());

        // Invalid master weight (> 255)
        let invalid_weight = SetOptionsOp {
            master_weight: Some(256),
            ..valid.clone()
        };
        assert!(validate_set_options(&invalid_weight, 24, None).is_err());

        // Invalid low threshold (> 255)
        let invalid_low = SetOptionsOp {
            low_threshold: Some(256),
            ..valid.clone()
        };
        assert!(validate_set_options(&invalid_low, 24, None).is_err());

        // Invalid med threshold (> 255)
        let invalid_med = SetOptionsOp {
            med_threshold: Some(256),
            ..valid.clone()
        };
        assert!(validate_set_options(&invalid_med, 24, None).is_err());

        // Invalid high threshold (> 255)
        let invalid_high = SetOptionsOp {
            high_threshold: Some(256),
            ..valid
        };
        assert!(validate_set_options(&invalid_high, 24, None).is_err());
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
        assert!(validate_change_trust(&valid, 24, None).is_ok());

        // Negative limit
        let negative = ChangeTrustOp {
            limit: -100,
            ..valid
        };
        assert!(validate_change_trust(&negative, 24, None).is_err());
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
        assert!(validate_manage_data(&valid, 21).is_ok());

        // Delete operation (None value) is also valid
        let delete = ManageDataOp {
            data_name: stellar_xdr::curr::String64::try_from(b"test".to_vec()).unwrap(),
            data_value: None,
        };
        assert!(validate_manage_data(&delete, 21).is_ok());

        // Invalid string (contains null byte)
        let invalid_name = ManageDataOp {
            data_name: stellar_xdr::curr::String64::try_from(b"te\x00st".to_vec()).unwrap(),
            data_value: None,
        };
        assert!(validate_manage_data(&invalid_name, 21).is_err());
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

    #[test]
    fn test_validate_account_merge_self_merge() {
        let src = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let dest = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        assert!(validate_account_merge(&dest, Some(&src)).is_err());
    }

    #[test]
    fn test_validate_account_merge_valid() {
        let src = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let dest = MuxedAccount::Ed25519(Uint256([2u8; 32]));
        assert!(validate_account_merge(&dest, Some(&src)).is_ok());
    }

    #[test]
    fn test_validate_account_merge_no_source() {
        let dest = MuxedAccount::Ed25519(Uint256([1u8; 32]));
        // Without source_account, check is skipped
        assert!(validate_account_merge(&dest, None).is_ok());
    }

    #[test]
    fn test_validate_begin_sponsoring_self_sponsorship() {
        let src = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let op = BeginSponsoringFutureReservesOp {
            sponsored_id: src.clone(),
        };
        assert!(validate_begin_sponsoring_future_reserves(&op, Some(&src)).is_err());
    }

    #[test]
    fn test_validate_begin_sponsoring_valid() {
        let src = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let other = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([2u8; 32])));
        let op = BeginSponsoringFutureReservesOp {
            sponsored_id: other,
        };
        assert!(validate_begin_sponsoring_future_reserves(&op, Some(&src)).is_ok());
    }

    #[test]
    fn test_validate_classic_op_structure_valid_payment() {
        let src = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: MuxedAccount::Ed25519(Uint256([2u8; 32])),
                asset: Asset::Native,
                amount: 1000,
            }),
        };
        assert!(validate_classic_op_structure(&op, 24, Some(&src)).is_ok());
    }

    #[test]
    fn test_validate_classic_op_structure_invalid_payment() {
        let src = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let op = Operation {
            source_account: None,
            body: OperationBody::Payment(PaymentOp {
                destination: MuxedAccount::Ed25519(Uint256([2u8; 32])),
                asset: Asset::Native,
                amount: 0, // invalid: zero amount
            }),
        };
        assert!(validate_classic_op_structure(&op, 24, Some(&src)).is_err());
    }

    #[test]
    fn test_validate_classic_op_structure_manage_buy_offer_malformed() {
        let src = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32])));
        let op = Operation {
            source_account: None,
            body: OperationBody::ManageBuyOffer(ManageBuyOfferOp {
                selling: Asset::Native,
                buying: Asset::Native, // invalid: same asset
                buy_amount: 100,
                price: Price { n: 1, d: 1 },
                offer_id: 0,
            }),
        };
        assert!(validate_classic_op_structure(&op, 24, Some(&src)).is_err());
    }

    #[test]
    fn test_malformed_operation_result_payment() {
        let body = OperationBody::Payment(PaymentOp {
            destination: MuxedAccount::Ed25519(Uint256([2u8; 32])),
            asset: Asset::Native,
            amount: 0,
        });
        let err = OperationValidationError::InvalidAmount(0);
        let result = malformed_operation_result(&body, &err);
        match result {
            OperationResult::OpInner(OperationResultTr::Payment(PaymentResult::Malformed)) => {}
            other => panic!("expected Payment::Malformed, got {:?}", other),
        }
    }

    #[test]
    fn test_malformed_operation_result_manage_buy_offer() {
        let body = OperationBody::ManageBuyOffer(ManageBuyOfferOp {
            selling: Asset::Native,
            buying: Asset::Native,
            buy_amount: 100,
            price: Price { n: 1, d: 1 },
            offer_id: 0,
        });
        let err = OperationValidationError::InvalidAsset("same assets".to_string());
        let result = malformed_operation_result(&body, &err);
        match result {
            OperationResult::OpInner(OperationResultTr::ManageBuyOffer(
                ManageBuyOfferResult::Malformed,
            )) => {}
            other => panic!("expected ManageBuyOffer::Malformed, got {:?}", other),
        }
    }

    #[test]
    fn test_malformed_operation_result_bump_sequence() {
        let body = OperationBody::BumpSequence(BumpSequenceOp {
            bump_to: SequenceNumber(-1),
        });
        let err = OperationValidationError::Other("bad seq".to_string());
        let result = malformed_operation_result(&body, &err);
        match result {
            OperationResult::OpInner(OperationResultTr::BumpSequence(
                BumpSequenceResult::BadSeq,
            )) => {}
            other => panic!("expected BumpSequence::BadSeq, got {:?}", other),
        }
    }

    #[test]
    fn test_malformed_operation_result_create_passive_sell_offer() {
        let body = OperationBody::CreatePassiveSellOffer(CreatePassiveSellOfferOp {
            selling: Asset::Native,
            buying: Asset::Native,
            amount: 100,
            price: Price { n: 1, d: 1 },
        });
        let err = OperationValidationError::InvalidAsset("same assets".to_string());
        let result = malformed_operation_result(&body, &err);
        // CreatePassiveSellOffer must use ManageSellOffer discriminant (stellar-core parity)
        match result {
            OperationResult::OpInner(OperationResultTr::ManageSellOffer(
                ManageSellOfferResult::Malformed,
            )) => {}
            other => panic!(
                "expected ManageSellOffer::Malformed (discriminant 3), got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_malformed_operation_result_set_options_bad_signer_self() {
        let body = OperationBody::SetOptions(SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        });
        let err = OperationValidationError::InvalidSigner;
        let result = malformed_operation_result(&body, &err);
        match result {
            OperationResult::OpInner(OperationResultTr::SetOptions(
                SetOptionsResult::BadSigner,
            )) => {}
            other => panic!("expected SetOptions::BadSigner, got {:?}", other),
        }
    }

    #[test]
    fn test_malformed_operation_result_set_options_invalid_home_domain() {
        let body = OperationBody::SetOptions(SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        });
        let err = OperationValidationError::InvalidHomeDomain;
        let result = malformed_operation_result(&body, &err);
        match result {
            OperationResult::OpInner(OperationResultTr::SetOptions(
                SetOptionsResult::InvalidHomeDomain,
            )) => {}
            other => panic!("expected SetOptions::InvalidHomeDomain, got {:?}", other),
        }
    }

    #[test]
    fn test_malformed_operation_result_set_options_flag_overlap() {
        let body = OperationBody::SetOptions(SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        });
        let err = OperationValidationError::FlagOverlap;
        let result = malformed_operation_result(&body, &err);
        match result {
            OperationResult::OpInner(OperationResultTr::SetOptions(SetOptionsResult::BadFlags)) => {
            }
            other => panic!("expected SetOptions::BadFlags, got {:?}", other),
        }
    }

    #[test]
    fn test_malformed_operation_result_set_options_unknown_flags() {
        let body = OperationBody::SetOptions(SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        });
        let err = OperationValidationError::UnknownFlags;
        let result = malformed_operation_result(&body, &err);
        match result {
            OperationResult::OpInner(OperationResultTr::SetOptions(
                SetOptionsResult::UnknownFlag,
            )) => {}
            other => panic!("expected SetOptions::UnknownFlag, got {:?}", other),
        }
    }

    #[test]
    fn test_malformed_operation_result_set_options_threshold_out_of_range() {
        let body = OperationBody::SetOptions(SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        });
        let err = OperationValidationError::InvalidThreshold;
        let result = malformed_operation_result(&body, &err);
        match result {
            OperationResult::OpInner(OperationResultTr::SetOptions(
                SetOptionsResult::ThresholdOutOfRange,
            )) => {}
            other => panic!("expected SetOptions::ThresholdOutOfRange, got {:?}", other),
        }
    }

    #[test]
    fn test_malformed_operation_result_set_options_master_weight_out_of_range() {
        let body = OperationBody::SetOptions(SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        });
        let err = OperationValidationError::InvalidWeight;
        let result = malformed_operation_result(&body, &err);
        match result {
            OperationResult::OpInner(OperationResultTr::SetOptions(
                SetOptionsResult::ThresholdOutOfRange,
            )) => {}
            other => panic!("expected SetOptions::ThresholdOutOfRange, got {:?}", other),
        }
    }

    #[test]
    fn test_malformed_operation_result_revoke_sponsorship() {
        let body = OperationBody::RevokeSponsorship(RevokeSponsorshipOp::LedgerEntry(
            LedgerKey::Offer(LedgerKeyOffer {
                seller_id: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([1u8; 32]))),
                offer_id: 0,
            }),
        ));
        let err = OperationValidationError::InvalidOfferId;
        let result = malformed_operation_result(&body, &err);
        match result {
            OperationResult::OpInner(OperationResultTr::RevokeSponsorship(
                RevokeSponsorshipResult::Malformed,
            )) => {}
            other => panic!("expected RevokeSponsorship::Malformed, got {:?}", other),
        }
    }

    #[test]
    fn test_validate_set_options_signer_is_self() {
        let key = Uint256([1u8; 32]);
        let source = AccountId(PublicKey::PublicKeyTypeEd25519(key.clone()));
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: SignerKey::Ed25519(key),
                weight: 1,
            }),
        };
        let err = validate_set_options(&op, 24, Some(&source)).unwrap_err();
        assert!(
            matches!(err, OperationValidationError::InvalidSigner),
            "expected InvalidSigner, got {:?}",
            err
        );
    }

    #[test]
    fn test_validate_set_options_signer_weight_invalid() {
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: SignerKey::Ed25519(Uint256([2u8; 32])),
                weight: 256,
            }),
        };
        let err = validate_set_options(&op, 24, None).unwrap_err();
        assert!(
            matches!(err, OperationValidationError::InvalidSigner),
            "expected InvalidSigner, got {:?}",
            err
        );
    }

    #[test]
    fn test_validate_set_options_empty_signed_payload() {
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: Some(Signer {
                key: SignerKey::Ed25519SignedPayload(SignerKeyEd25519SignedPayload {
                    ed25519: Uint256([3u8; 32]),
                    payload: BytesM::default(),
                }),
                weight: 1,
            }),
        };
        let err = validate_set_options(&op, 24, None).unwrap_err();
        assert!(
            matches!(err, OperationValidationError::InvalidSigner),
            "expected InvalidSigner, got {:?}",
            err
        );
    }

    #[test]
    fn test_validate_set_options_invalid_home_domain() {
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: None,
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: Some(String32::try_from(vec![0x01]).unwrap()),
            signer: None,
        };
        let err = validate_set_options(&op, 24, None).unwrap_err();
        assert!(
            matches!(err, OperationValidationError::InvalidHomeDomain),
            "expected InvalidHomeDomain, got {:?}",
            err
        );
    }

    #[test]
    fn test_validate_set_options_unknown_flags() {
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: None,
            set_flags: Some(0x8000_0000),
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };
        let err = validate_set_options(&op, 24, None).unwrap_err();
        assert!(
            matches!(err, OperationValidationError::UnknownFlags),
            "expected UnknownFlags, got {:?}",
            err
        );
    }

    #[test]
    fn test_validate_set_options_flag_overlap() {
        let op = SetOptionsOp {
            inflation_dest: None,
            clear_flags: Some(1),
            set_flags: Some(1),
            master_weight: None,
            low_threshold: None,
            med_threshold: None,
            high_threshold: None,
            home_domain: None,
            signer: None,
        };
        let err = validate_set_options(&op, 24, None).unwrap_err();
        assert!(
            matches!(err, OperationValidationError::FlagOverlap),
            "expected FlagOverlap, got {:?}",
            err
        );
    }

    #[test]
    fn test_validate_classic_op_structure_skips_soroban() {
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash([0u8; 32]))),
                    function_name: ScSymbol("test".try_into().unwrap()),
                    args: VecM::default(),
                }),
                auth: VecM::default(),
            }),
        };
        // Soroban ops should pass through (no classic validation)
        assert!(validate_classic_op_structure(&op, 24, None).is_ok());
    }
}
